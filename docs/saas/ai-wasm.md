# AI and server-side WASM

> **Status:** target design · **Last updated:** 2026-10-16

## Goals

- Add useful AI capabilities without coupling FigCraft to one provider.
- Keep provider credentials, spend controls and policy on the server.
- Make every AI document mutation authorized, validated, synchronized, auditable and undoable.
- Reuse the command engine instead of introducing an alternate mutation model.
- Permit tenant automation through WASM without exposing the host filesystem or unrestricted
  network access.
- Keep AI and automation failures isolated from normal editing and collaboration.

## AI gateway

The gateway receives feature-level requests rather than exposing raw provider APIs as the primary
product contract. Initial features are:

| Feature | Input | Output |
|---|---|---|
| Layer naming | Selected layer summaries and visible hierarchy | Proposed rename command batch |
| Layer organization | Selected nodes, bounds and hierarchy | Proposed grouping, ordering and naming commands |
| Draft generation | Brief, target frame and design constraints | Command batch plus optional generated assets |
| Copy generation | Selected text context and requested tone | Text replacement proposal |
| Accessibility review | Inspect data and rendered preview | Findings with layer references and optional fixes |
| Visual explanation | Rendered preview and inspect data | Natural-language response with layer references |
| Image generation | Prompt, size, palette and policy | Verified object-storage asset |
| Image editing | Authorized source asset, mask and instruction | Verified derived asset |
| Background removal | Authorized source asset | Verified transparent asset |
| Semantic search | Query and tenant-authorized asset index | Ranked asset or document references |

Each feature defines its own context builder, output schema, maximum cost and allowed providers.

## Provider abstraction

Provider adapters implement operations rather than leaking provider request types into product
code:

```text
generate_text
generate_structured
understand_image
generate_image
edit_image
embed_text
embed_image
moderate
```

An adapter receives a normalized request and returns normalized usage, finish state and content.
Raw provider responses may be retained only when tenant policy permits and must never include
credentials in logs or errors.

Provider configuration supports:

- Platform-managed credentials.
- Tenant bring-your-own-key credentials.
- OpenAI-compatible private endpoints.
- Local model runtimes reachable only from the AI worker network.
- Feature-specific allowlists and fallback order.
- Model aliases so product behavior does not depend on a vendor model name.
- Per-request and per-period limits.

## AI job lifecycle

1. An authorized principal submits a feature request with document and selection identifiers.
2. The API resolves tenant policy, feature availability, quota and estimated maximum cost.
3. A durable job records immutable input references and the requested document sequence.
4. The worker loads only the authorized context needed by the feature.
5. The gateway selects an allowed provider and records the attempt.
6. The provider result is checked against size and structured-output limits.
7. Proposed commands are validated by the FigCraft engine against the requested document state.
8. The job either returns a preview/proposal or submits the batch to the authoritative relay.
9. The relay rechecks authorization and current document state, commits the event and attributes it
   to the AI service principal.
10. Usage, cost, outcome and audit metadata are finalized.

Jobs are cancellable before document commit. A completed mutation is reverted through normal undo
or a compensating command, never by deleting event rows.

## Context policy

- The default context is the selected nodes, their necessary ancestors, relevant styles and a
  bounded render.
- Whole-document context requires a feature-specific reason and tenant permission.
- Hidden pages, comments and version history are excluded unless the feature explicitly needs them.
- External libraries are represented by referenced metadata unless access to their source is also
  authorized.
- Context builders cap node count, string length, image dimensions and encoded bytes.
- Provider-bound content is recorded by category and size for audit without duplicating sensitive
  document content in general logs.
- Tenants may disable external providers or restrict processing to approved regions or local
  endpoints.

## Structured command output

Document-changing AI features return a schema containing:

```json
{
  "documentSequence": 1842,
  "summary": "Organize the selected checkout form",
  "commands": [
    {
      "command": "object.group",
      "params": {
        "ids": [42, 43, 44]
      }
    },
    {
      "command": "node.set",
      "params": {
        "ids": [45],
        "name": "Checkout form"
      }
    }
  ]
}
```

The gateway rejects unknown commands, disallowed commands, invalid parameters, excessive batch
sizes and references outside the authorized document. The engine executes the batch behind its
existing guard. The relay applies it as one named undo step.

The model does not receive direct database, MinIO or collaboration credentials.

## AI participant identity

Each AI mutation records:

- Tenant.
- Requesting principal.
- AI service principal.
- Feature.
- Provider and model alias.
- Input document sequence.
- Applied document sequence.
- Command count and command identifiers.
- Usage and computed cost.
- Request and trace identifiers.
- Success, rejection, cancellation or failure.

The live session presents the service principal as a distinct participant when it applies a batch.
Its edits remain attributable to the requesting user and have their own undo step.

## Generated assets

1. Reserve an output object within the tenant.
2. Generate into bounded worker memory or a private staging stream.
3. Decode and validate dimensions, media type and resource limits.
4. Apply moderation and tenant policy.
5. Compute the content hash.
6. Upload to the tenant staging prefix.
7. Verify checksum and promote to the content-addressed asset key.
8. Attach the object through a normal document command.

Failed or rejected assets remain inaccessible and expire through staging lifecycle rules.

## Usage and billing

Usage is recorded per attempt and aggregated per billing period. Units include provider input and
output tokens, image operations, embedding units, worker time and stored generated bytes. The API
reserves a maximum allowance before starting expensive work and settles actual usage afterward.

Limits apply at tenant, workspace, principal and feature levels. Hard-limit exhaustion rejects new
jobs. Soft limits produce notifications and metrics without silently exceeding a configured hard
budget.

## Data retention

Tenant policy selects one of these modes:

| Mode | Retention |
|---|---|
| Minimal | Feature metadata, usage, command result and hashes; no prompt or provider response body |
| Diagnostic | Redacted prompt and structured result for a bounded period |
| Full audit | Encrypted prompt and response for a configured period with restricted access |

Provider-side retention requirements are shown when a provider is configured. A tenant policy may
exclude providers that cannot satisfy its requirements.

## Server-side WASM runtime

Server automation is distinct from the existing client plugin runtime. Modules run only in worker
processes or dedicated worker containers.

### Module lifecycle

1. Publisher uploads a bundle through a verified reservation.
2. The service validates the manifest, API version, imports, size and cryptographic checksum.
3. An administrator or authorized tenant publisher approves requested capabilities.
4. The immutable bundle is stored in MinIO.
5. A job selects a specific approved version.
6. The worker loads it into a fresh metered runtime.
7. Host calls enforce tenant, document, capability and size limits.
8. Mutations are accumulated as a command batch.
9. Successful execution submits one batch; failure submits none.

### Capability model

| Capability | Access |
|---|---|
| `document.inspect` | Bounded JSON inspection of the authorized document or selection |
| `document.command` | Approved FigCraft command identifiers and parameter limits |
| `asset.read` | Bounded read of specifically referenced tenant assets |
| `asset.write` | Create a staging object subject to verification and quota |
| `ai.request` | Submit an allowed feature request through the AI gateway |
| `log.write` | Structured, size-limited logs associated with the job |
| `job.progress` | Bounded progress updates |
| `kv.read`, `kv.write` | Small tenant/module-scoped settings, with quota |

No capability provides arbitrary SQL, Redis, MinIO, filesystem, process, clock precision or network
access. A coarse platform time may be supplied when needed for deterministic business logic.

### Runtime limits

- Maximum bundle and decoded module size.
- Maximum linear memory.
- Instruction fuel.
- Wall-clock deadline enforced outside the module.
- Maximum host calls and bytes copied per call.
- Maximum log lines and bytes.
- Maximum command count and serialized parameters.
- Maximum asset input and output bytes.
- Maximum concurrent jobs per tenant and module.

Limits are stored with the approved module version so a retry uses the same policy.

## Network isolation

- API and relay containers do not run user WASM.
- WASM workers use a restricted network policy.
- Provider traffic is available only to the AI gateway or AI worker.
- Local model runtimes accept traffic only from the AI service network.
- MinIO administration is unavailable to workers; object access uses service-scoped credentials or
  short-lived requests.
- Metadata endpoints of cloud providers are blocked from plugin and AI execution networks.

## AI and WASM tests

- Provider contract tests with deterministic fake providers.
- Structured-output fuzz tests.
- Command allowlist and cross-document reference tests.
- Tenant context minimization snapshots.
- Prompt-injection tests against document text and plugin output.
- Provider timeout, retry and partial-response tests.
- Quota reservation and settlement race tests.
- Generated-image size and decoder adversarial tests.
- WASM fuel, memory, deadline, host-call and output-limit tests.
- Capability escalation and cross-tenant access tests.
- Worker termination before and after asset upload and document commit.
- Audit completeness tests for every success and failure outcome.

## Release gates

- No provider secret appears in API responses, logs, traces, job payloads or panic reports.
- A model cannot reference or modify a node outside its authorized document.
- AI commands cannot bypass the relay's durable commit path.
- A failed AI or WASM job leaves the document unchanged.
- Generated assets are inaccessible until validation completes.
- Quota accounting remains correct across retry, cancellation and worker termination.
- Disabling a provider or module prevents new execution immediately.
- Tenant prompt-retention policy is verified by integration tests.
