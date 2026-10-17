//! Syntax-level extraction with tree-sitter: definitions, name occurrences, imports and impls.
//!
//! Everything here is syntax-derived. Occurrences are recorded by name only (`provenance =
//! syntax`) and never claimed to be resolved; the SCIP import adds resolved targets.

use crate::inventory::Lang;
use tree_sitter::{Node, Parser};

/// Deeper nesting than this is skipped rather than risking the stack.
const MAX_DEPTH: usize = 400;
/// Bytes of each definition body kept for full-text search.
const BODY_FTS_BYTES: usize = 2000;
const SIG_CHARS: usize = 240;
const DOC_CHARS: usize = 240;

#[derive(Debug, Clone, Default)]
pub struct Sym {
    pub name: String,
    pub qualname: String,
    pub kind: &'static str,
    pub parent: Option<usize>,
    pub start_line: u32,
    pub end_line: u32,
    pub name_line: u32,
    pub name_col: u32,
    pub sig: String,
    pub doc: String,
    pub is_test: bool,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Ref {
    pub line: u32,
    pub col: u32,
    pub name: String,
    pub kind: &'static str,
    pub enclosing: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub line: u32,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct Impl {
    pub symbol: usize,
    pub trait_name: String,
    pub trait_path: String,
    pub type_name: String,
    pub line: u32,
}

#[derive(Debug, Default)]
pub struct Facts {
    pub symbols: Vec<Sym>,
    pub refs: Vec<Ref>,
    pub imports: Vec<Import>,
    pub impls: Vec<Impl>,
    pub lines: u32,
}

pub fn extract(lang: Lang, module: &str, file_is_test: bool, src: &str) -> Facts {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    };
    let facts = Facts { lines: u32::try_from(src.lines().count()).unwrap_or(u32::MAX), ..Facts::default() };
    if parser.set_language(&language).is_err() {
        return facts;
    }
    let Some(tree) = parser.parse(src, None) else { return facts };
    let mut x = Extractor { src: src.as_bytes(), facts, lang };
    let ctx = Ctx { module: module.to_string(), owner: None, parent: None, is_test: file_is_test, in_impl_or_trait: false };
    x.walk(tree.root_node(), &ctx, 0);
    x.facts
}

#[derive(Clone)]
struct Ctx {
    /// Qualified path new definitions are named under.
    module: String,
    /// Type or trait name methods belong to (`Type` in `impl Trait for Type`).
    owner: Option<String>,
    /// Innermost enclosing symbol index.
    parent: Option<usize>,
    is_test: bool,
    in_impl_or_trait: bool,
}

struct Extractor<'a> {
    src: &'a [u8],
    facts: Facts,
    lang: Lang,
}

fn line(n: &Node) -> u32 {
    u32::try_from(n.start_position().row).unwrap_or(u32::MAX).saturating_add(1)
}

fn end_line(n: &Node) -> u32 {
    u32::try_from(n.end_position().row).unwrap_or(u32::MAX).saturating_add(1)
}

fn col(n: &Node) -> u32 {
    u32::try_from(n.start_position().column).unwrap_or(u32::MAX)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("").to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `Foo<T>` / `a::b::Foo<T>` / `&mut Foo` -> `Foo`.
fn base_name(s: &str) -> String {
    let s = s.split('<').next().unwrap_or(s);
    let s = s.trim_start_matches(['&', '*']).trim_start_matches("mut ").trim_start_matches("dyn ").trim();
    s.rsplit("::").next().unwrap_or(s).trim().to_string()
}

impl Extractor<'_> {
    fn text(&self, n: &Node) -> &str {
        n.utf8_text(self.src).unwrap_or("")
    }

    fn field_text(&self, n: &Node, field: &str) -> Option<String> {
        n.child_by_field_name(field).map(|c| self.text(&c).to_string())
    }

    /// Signature: the definition's text up to its body, whitespace collapsed.
    fn signature(&self, n: &Node) -> String {
        let t = self.text(n);
        let cut = match n.child_by_field_name("body") {
            Some(b) => {
                let off = b.start_byte().saturating_sub(n.start_byte());
                t.get(..off).unwrap_or(t)
            }
            None => t.split('{').next().unwrap_or(t),
        };
        truncate(&collapse_ws(cut.trim_end_matches(['{', ' ', '\n'])), SIG_CHARS)
    }

    /// Doc comments and test attributes directly above a definition.
    fn leading(&self, n: &Node) -> (String, bool) {
        let mut docs: Vec<String> = Vec::new();
        let mut is_test = false;
        let mut prev = n.prev_sibling();
        while let Some(p) = prev {
            match p.kind() {
                "attribute_item" => {
                    let t = self.text(&p);
                    if t.contains("test") {
                        is_test = true;
                    }
                }
                "line_comment" | "block_comment" | "comment" => {
                    let t = self.text(&p).trim();
                    let doc = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")).map(|d| d.trim().to_string()).or_else(|| {
                        t.strip_prefix("/**")
                            .map(|d| d.trim_end_matches("*/").lines().map(|l| l.trim().trim_start_matches('*').trim()).collect::<Vec<_>>().join(" "))
                    });
                    match doc {
                        Some(d) => docs.push(d),
                        None => break,
                    }
                }
                _ => break,
            }
            prev = p.prev_sibling();
        }
        docs.reverse();
        (truncate(&collapse_ws(&docs.join(" ")), DOC_CHARS), is_test)
    }

    fn push_symbol(&mut self, n: &Node, name_node: &Node, name: String, kind: &'static str, qualname: String, ctx: &Ctx) -> usize {
        let (doc, attr_test) = self.leading(n);
        let body = truncate(self.text(n), BODY_FTS_BYTES);
        self.facts.symbols.push(Sym {
            name,
            qualname,
            kind,
            parent: ctx.parent,
            start_line: line(n),
            end_line: end_line(n),
            name_line: line(name_node),
            name_col: col(name_node),
            sig: self.signature(n),
            doc,
            is_test: ctx.is_test || attr_test,
            body,
        });
        self.facts.symbols.len() - 1
    }

    fn push_ref(&mut self, n: &Node, name: &str, kind: &'static str, ctx: &Ctx) {
        let name = name.trim();
        if name.is_empty() || name.len() > 120 {
            return;
        }
        self.facts.refs.push(Ref { line: line(n), col: col(n), name: name.to_string(), kind, enclosing: ctx.parent });
    }

    fn walk_children(&mut self, n: Node, ctx: &Ctx, depth: usize) {
        let mut cursor = n.walk();
        let children: Vec<Node> = n.children(&mut cursor).collect();
        for c in children {
            self.walk(c, ctx, depth + 1);
        }
    }

    fn walk(&mut self, n: Node, ctx: &Ctx, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match self.lang {
            Lang::Rust => self.rust(n, ctx, depth),
            Lang::Ts | Lang::Tsx => self.ts(n, ctx, depth),
        }
    }

    // ---------------------------------------------------------------------------------- Rust

    fn rust(&mut self, n: Node, ctx: &Ctx, depth: usize) {
        let kind = n.kind();
        let def_kind: Option<&'static str> = match kind {
            "function_item" | "function_signature_item" => Some(if ctx.in_impl_or_trait { "method" } else { "fn" }),
            "struct_item" => Some("struct"),
            "enum_item" => Some("enum"),
            "union_item" => Some("union"),
            "trait_item" => Some("trait"),
            "type_item" => Some("type"),
            "const_item" => Some("const"),
            "static_item" => Some("static"),
            "macro_definition" => Some("macro"),
            "mod_item" => Some("mod"),
            "enum_variant" => Some("variant"),
            _ => None,
        };
        if let Some(dk) = def_kind {
            let Some(name_node) = n.child_by_field_name("name") else { return self.walk_children(n, ctx, depth) };
            let name = self.text(&name_node).to_string();
            let qual_base = match (&ctx.owner, dk) {
                (Some(owner), "method" | "const" | "type" | "variant") => format!("{}::{}", ctx.module, owner),
                _ => ctx.module.clone(),
            };
            let qualname = format!("{qual_base}::{name}");
            let idx = self.push_symbol(&n, &name_node, name.clone(), dk, qualname.clone(), ctx);
            let is_test = self.facts.symbols.get(idx).is_some_and(|s| s.is_test);
            let mut inner = Ctx { parent: Some(idx), is_test, ..ctx.clone() };
            match dk {
                "mod" => {
                    inner.module = qualname;
                    inner.owner = None;
                    inner.in_impl_or_trait = false;
                }
                "trait" => {
                    inner.owner = Some(name);
                    inner.in_impl_or_trait = true;
                }
                "enum" => {
                    inner.owner = Some(name);
                }
                "fn" | "method" => {
                    // Items nested in a function body are named under the function.
                    inner.owner = None;
                    inner.in_impl_or_trait = false;
                    inner.module = qualname;
                }
                _ => {}
            }
            // Walk everything but the definition's own name.
            let mut cursor = n.walk();
            let children: Vec<Node> = n.children(&mut cursor).collect();
            for c in children {
                if c.id() != name_node.id() {
                    self.walk(c, &inner, depth + 1);
                }
            }
            return;
        }
        match kind {
            "impl_item" => self.rust_impl(n, ctx, depth),
            "use_declaration" => {
                if let Some(arg) = n.child_by_field_name("argument") {
                    let mut paths = Vec::new();
                    self.expand_use(&arg, "", &mut paths);
                    let l = line(&n);
                    self.facts.imports.extend(paths.into_iter().map(|path| Import { line: l, path }));
                }
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    self.rust_callee(&f, ctx);
                }
                self.walk_children(n, ctx, depth);
            }
            "macro_invocation" => {
                if let Some(m) = n.child_by_field_name("macro") {
                    let name = match m.kind() {
                        "scoped_identifier" => self.field_text(&m, "name").unwrap_or_default(),
                        _ => self.text(&m).to_string(),
                    };
                    self.push_ref(&m, &name, "macro", ctx);
                }
                self.walk_children(n, ctx, depth);
            }
            "type_identifier" => {
                let t = self.text(&n).to_string();
                self.push_ref(&n, &t, "type", ctx);
            }
            // Macro arguments are raw tokens to tree-sitter: recover `f(…)`, `.m(…)`, `T::f(…)`
            // and capitalised type names from the token sequence.
            "token_tree" => self.rust_token_tree(n, ctx, depth),
            "scoped_identifier" | "scoped_type_identifier" => {
                let parent_kind = n.parent().map(|p| p.kind()).unwrap_or("");
                let is_callee = n.parent().and_then(|p| p.child_by_field_name("function")).is_some_and(|f| f.id() == n.id());
                if !is_callee
                    && parent_kind != "scoped_identifier"
                    && parent_kind != "scoped_type_identifier"
                    && let Some(name) = n.child_by_field_name("name")
                {
                    let t = self.text(&name).to_string();
                    let k = if kind == "scoped_type_identifier" { "type" } else { "path" };
                    self.push_ref(&name, &t, k, ctx);
                }
                // Generic arguments inside the path still hold type references.
                let mut cursor = n.walk();
                let children: Vec<Node> = n.children(&mut cursor).collect();
                for c in children {
                    if c.kind() == "type_arguments" {
                        self.walk(c, ctx, depth + 1);
                    }
                }
            }
            _ => self.walk_children(n, ctx, depth),
        }
    }

    fn rust_token_tree(&mut self, n: Node, ctx: &Ctx, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut cursor = n.walk();
        let toks: Vec<Node> = n.children(&mut cursor).collect();
        for (i, t) in toks.iter().enumerate() {
            match t.kind() {
                "token_tree" => self.rust_token_tree(*t, ctx, depth + 1),
                "identifier" => {
                    let name = self.text(t).to_string();
                    let next_is_call = toks.get(i + 1).is_some_and(|nx| nx.kind() == "token_tree" && self.text(nx).starts_with('('));
                    let prev = if i > 0 { toks.get(i - 1).map(|p| self.text(p)) } else { None };
                    if next_is_call {
                        let kind = if prev == Some(".") { "method" } else { "call" };
                        self.push_ref(t, &name, kind, ctx);
                    } else if name.chars().next().is_some_and(char::is_uppercase) && prev != Some(".") {
                        self.push_ref(t, &name, "type", ctx);
                    } else if prev == Some("::") {
                        self.push_ref(t, &name, "path", ctx);
                    }
                }
                _ => {}
            }
        }
    }

    fn rust_callee(&mut self, f: &Node, ctx: &Ctx) {
        match f.kind() {
            "identifier" => {
                let t = self.text(f).to_string();
                self.push_ref(f, &t, "call", ctx);
            }
            "scoped_identifier" => {
                if let Some(name) = f.child_by_field_name("name") {
                    let t = self.text(&name).to_string();
                    self.push_ref(&name, &t, "call", ctx);
                }
            }
            "field_expression" => {
                if let Some(field) = f.child_by_field_name("field") {
                    let t = self.text(&field).to_string();
                    self.push_ref(&field, &t, "method", ctx);
                }
            }
            "generic_function" => {
                if let Some(inner) = f.child_by_field_name("function") {
                    self.rust_callee(&inner, ctx);
                }
            }
            _ => {}
        }
    }

    fn rust_impl(&mut self, n: Node, ctx: &Ctx, depth: usize) {
        let Some(ty) = n.child_by_field_name("type") else { return self.walk_children(n, ctx, depth) };
        let type_text = collapse_ws(self.text(&ty));
        let type_name = base_name(&type_text);
        let trait_node = n.child_by_field_name("trait");
        let trait_text = trait_node.map(|t| collapse_ws(self.text(&t)));
        let name = match &trait_text {
            Some(t) => format!("impl {} for {}", base_name(t), type_name),
            None => format!("impl {type_name}"),
        };
        let qualname = format!("{}::{}", ctx.module, name);
        let idx = self.push_symbol(&n, &ty, name, "impl", qualname, ctx);
        if let Some(t) = &trait_text {
            self.facts.impls.push(Impl {
                symbol: idx,
                trait_name: base_name(t),
                trait_path: t.split('<').next().unwrap_or(t).trim().to_string(),
                type_name: type_name.clone(),
                line: line(&n),
            });
        }
        let is_test = self.facts.symbols.get(idx).is_some_and(|s| s.is_test);
        let inner = Ctx { parent: Some(idx), owner: Some(type_name), in_impl_or_trait: true, is_test, ..ctx.clone() };
        // The trait and type names are references too.
        if let Some(t) = trait_node {
            self.walk(t, ctx, depth + 1);
        }
        self.walk(ty, ctx, depth + 1);
        if let Some(body) = n.child_by_field_name("body") {
            self.walk_children(body, &inner, depth + 1);
        }
    }

    /// Expands a `use` tree into full paths: `a::{b, c::d as e}` -> `a::b`, `a::c::d`.
    fn expand_use(&self, n: &Node, prefix: &str, out: &mut Vec<String>) {
        if out.len() > 500 {
            return;
        }
        let join = |p: &str, s: &str| if p.is_empty() { s.to_string() } else { format!("{p}::{s}") };
        match n.kind() {
            "scoped_use_list" => {
                let path = n.child_by_field_name("path").map(|p| self.text(&p).to_string()).unwrap_or_default();
                let pre = join(prefix, &path);
                if let Some(list) = n.child_by_field_name("list") {
                    self.expand_use(&list, &pre, out);
                }
            }
            "use_list" => {
                let mut cursor = n.walk();
                let children: Vec<Node> = n.named_children(&mut cursor).collect();
                for c in children {
                    self.expand_use(&c, prefix, out);
                }
            }
            "use_as_clause" => {
                if let Some(p) = n.child_by_field_name("path") {
                    out.push(join(prefix, self.text(&p)));
                }
            }
            "use_wildcard" => {
                let t = self.text(n).trim_end_matches("::*").trim_end_matches('*').trim_end_matches("::").to_string();
                out.push(join(prefix, &t));
            }
            "self" => out.push(prefix.to_string()),
            _ => out.push(join(prefix, &collapse_ws(self.text(n)))),
        }
    }

    // ---------------------------------------------------------------------------- TypeScript

    fn ts(&mut self, n: Node, ctx: &Ctx, depth: usize) {
        let kind = n.kind();
        let def_kind: Option<&'static str> = match kind {
            "function_declaration" | "generator_function_declaration" | "function_signature" => Some("fn"),
            "class_declaration" | "abstract_class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            "type_alias_declaration" => Some("type"),
            "enum_declaration" => Some("enum"),
            "method_definition" | "method_signature" | "abstract_method_signature" => Some("method"),
            _ => None,
        };
        if let Some(dk) = def_kind {
            let Some(name_node) = n.child_by_field_name("name") else { return self.walk_children(n, ctx, depth) };
            let name = self.text(&name_node).to_string();
            let qual_base = match (&ctx.owner, dk) {
                (Some(owner), "method") => format!("{}::{}", ctx.module, owner),
                _ => ctx.module.clone(),
            };
            let qualname = format!("{qual_base}::{name}");
            let idx = self.push_symbol(&n, &name_node, name.clone(), dk, qualname.clone(), ctx);
            if matches!(dk, "class" | "interface") {
                self.ts_heritage(&n, idx, &name);
            }
            let mut inner = Ctx { parent: Some(idx), ..ctx.clone() };
            if matches!(dk, "class" | "interface") {
                inner.owner = Some(name);
            } else if matches!(dk, "fn" | "method") {
                inner.owner = None;
                inner.module = qualname;
            }
            let mut cursor = n.walk();
            let children: Vec<Node> = n.children(&mut cursor).collect();
            for c in children {
                if c.id() != name_node.id() {
                    self.walk(c, &inner, depth + 1);
                }
            }
            return;
        }
        match kind {
            "variable_declarator" => {
                // `const f = () => …` / `const f = function …` are functions; other top-level
                // constants are recorded as constants.
                let value_kind = n.child_by_field_name("value").map(|v| v.kind()).unwrap_or("");
                let top_level = ctx.parent.is_none();
                let fn_like = matches!(value_kind, "arrow_function" | "function_expression" | "function");
                match n.child_by_field_name("name") {
                    Some(name_node) if name_node.kind() == "identifier" && (fn_like || top_level) => {
                        let name = self.text(&name_node).to_string();
                        let dk = if fn_like { "fn" } else { "const" };
                        let qualname = format!("{}::{}", ctx.module, name);
                        let idx = self.push_symbol(&n, &name_node, name, dk, qualname.clone(), ctx);
                        let inner = Ctx { parent: Some(idx), module: qualname, owner: None, ..ctx.clone() };
                        if let Some(v) = n.child_by_field_name("value") {
                            self.walk(v, &inner, depth + 1);
                        }
                    }
                    _ => self.walk_children(n, ctx, depth),
                }
            }
            "import_statement" => {
                if let Some(src) = n.child_by_field_name("source") {
                    let path = self.text(&src).trim_matches(['"', '\'', '`']).to_string();
                    self.facts.imports.push(Import { line: line(&n), path });
                }
            }
            "call_expression" | "new_expression" => {
                let f = n.child_by_field_name("function").or_else(|| n.child_by_field_name("constructor"));
                if let Some(f) = f {
                    match f.kind() {
                        "identifier" => {
                            let t = self.text(&f).to_string();
                            self.push_ref(&f, &t, "call", ctx);
                        }
                        "member_expression" => {
                            if let Some(p) = f.child_by_field_name("property") {
                                let t = self.text(&p).to_string();
                                self.push_ref(&p, &t, "method", ctx);
                            }
                        }
                        _ => {}
                    }
                }
                self.walk_children(n, ctx, depth);
            }
            "type_identifier" => {
                let t = self.text(&n).to_string();
                self.push_ref(&n, &t, "type", ctx);
            }
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let t = self.text(&name).to_string();
                    if t.chars().next().is_some_and(char::is_uppercase) {
                        self.push_ref(&name, &t, "call", ctx);
                    }
                }
                self.walk_children(n, ctx, depth);
            }
            _ => self.walk_children(n, ctx, depth),
        }
    }

    /// `class X implements A, B` / `interface I extends J` -> impl rows.
    fn ts_heritage(&mut self, n: &Node, idx: usize, type_name: &str) {
        let mut stack = vec![*n];
        let mut seen = 0;
        while let Some(cur) = stack.pop() {
            seen += 1;
            if seen > 200 {
                break;
            }
            let mut cursor = cur.walk();
            for c in cur.children(&mut cursor) {
                match c.kind() {
                    "class_heritage" | "implements_clause" | "extends_clause" | "extends_type_clause" => stack.push(c),
                    "type_identifier" | "identifier" if cur.id() != n.id() => {
                        let t = self.text(&c).to_string();
                        self.facts.impls.push(Impl {
                            symbol: idx,
                            trait_name: t.clone(),
                            trait_path: t,
                            type_name: type_name.to_string(),
                            line: line(&c),
                        });
                    }
                    "generic_type" if cur.id() != n.id() => {
                        if let Some(name) = c.child_by_field_name("name") {
                            let t = self.text(&name).to_string();
                            self.facts.impls.push(Impl {
                                symbol: idx,
                                trait_name: t.clone(),
                                trait_path: t,
                                type_name: type_name.to_string(),
                                line: line(&c),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Splits identifiers into words for full-text search: `RoomLease` -> `room lease`.
pub fn words(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch == '_' || ch == ':' || ch == '-' || ch == '/' || ch == '.' {
            out.push(' ');
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        out.extend(ch.to_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_definitions_refs_and_imports() {
        let src = r#"
use crate::a::{B, c::D as E};
/// Does a thing.
pub fn run(x: u32) -> u32 { helper(x) + S::new().go() }
pub struct S;
impl Display for S { fn fmt(&self) {} }
impl S { pub fn new() -> Self { S } }
#[cfg(test)]
mod tests { #[test] fn t() { super::run(1); } }
"#;
        let f = extract(Lang::Rust, "k::m", false, src);
        let names: Vec<_> = f.symbols.iter().map(|s| (s.kind, s.qualname.as_str())).collect();
        assert!(names.contains(&("fn", "k::m::run")), "{names:?}");
        assert!(names.contains(&("method", "k::m::S::new")), "{names:?}");
        assert!(names.contains(&("method", "k::m::S::fmt")), "{names:?}");
        assert!(names.contains(&("fn", "k::m::tests::t")), "{names:?}");
        let run = f.symbols.iter().find(|s| s.name == "run").map(|s| s.doc.clone());
        assert_eq!(run.as_deref(), Some("Does a thing."));
        assert!(f.symbols.iter().find(|s| s.name == "t").is_some_and(|s| s.is_test));
        let imports: Vec<_> = f.imports.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(imports, vec!["crate::a::B", "crate::a::c::D"]);
        assert!(f.refs.iter().any(|r| r.name == "helper" && r.kind == "call"));
        assert!(f.refs.iter().any(|r| r.name == "go" && r.kind == "method"));
        assert!(f.refs.iter().any(|r| r.name == "new" && r.kind == "call"));
        assert_eq!(f.impls.len(), 1);
        assert_eq!(f.impls.first().map(|i| i.trait_name.as_str()), Some("Display"));
    }

    #[test]
    fn typescript_definitions() {
        let src = "import { x } from './x';\nexport class A implements B { go() { return x(); } }\nexport const f = () => new A();\n";
        let f = extract(Lang::Ts, "web/a", false, src);
        let names: Vec<_> = f.symbols.iter().map(|s| (s.kind, s.qualname.as_str())).collect();
        assert!(names.contains(&("class", "web/a::A")), "{names:?}");
        assert!(names.contains(&("method", "web/a::A::go")), "{names:?}");
        assert!(names.contains(&("fn", "web/a::f")), "{names:?}");
        assert_eq!(f.imports.first().map(|i| i.path.as_str()), Some("./x"));
        assert_eq!(f.impls.first().map(|i| i.trait_name.as_str()), Some("B"));
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let deep = format!("fn f() {{ m!({}{}); let x = {}1{}; }}", "(".repeat(5000), ")".repeat(5000), "(".repeat(5000), ")".repeat(5000));
        let f = extract(Lang::Rust, "k", false, &deep);
        assert_eq!(f.symbols.len(), 1);
        let ts = format!("const a = {}1{};", "[".repeat(5000), "]".repeat(5000));
        assert_eq!(extract(Lang::Ts, "m", false, &ts).symbols.len(), 1);
    }

    #[test]
    fn words_split_identifiers() {
        assert_eq!(words("RoomLease::acquire_lease"), "room lease  acquire lease");
    }
}
