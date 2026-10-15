/** Major version of the browser-shell to Rust/WASM lifecycle contract. */
export const EDITOR_BRIDGE_PROTOCOL_VERSION = 1 as const;

export type EditorBridgeProtocolVersion = typeof EDITOR_BRIDGE_PROTOCOL_VERSION;

declare const cloudDocumentIdBrand: unique symbol;
declare const editorSessionTicketBrand: unique symbol;
declare const opaqueBinaryBrand: unique symbol;

/** Stable cloud identity supplied by the application API. Its syntax is not interpreted here. */
export type CloudDocumentId = string & {
  readonly [cloudDocumentIdBrand]: "CloudDocumentId";
};

/** Short-lived, document-scoped credential. It must never be persisted by the browser shell. */
export type EditorSessionTicket = string & {
  readonly [editorSessionTicketBrand]: "EditorSessionTicket";
};

/**
 * Serialized data owned by Rust.
 *
 * TypeScript may forward or retain the complete envelope, but it must not parse document changes,
 * rewrite bytes or derive application behavior from its contents.
 */
export interface OpaqueBinary {
  readonly [opaqueBinaryBrand]: true;
  readonly mediaType: "application/vnd.figcraft.binary";
  readonly encodingVersion: number;
  readonly bytes: Uint8Array;
}

export type EditorConnectionState =
  | "connecting"
  | "online"
  | "reconnecting"
  | "offline"
  | "closed";

export type EditorSyncState = "saved" | "saving" | "offline" | "conflict" | "error";

export type EditorEvent =
  | {
      readonly kind: "ready";
      readonly protocolVersion: EditorBridgeProtocolVersion;
    }
  | {
      readonly kind: "connection-state";
      readonly state: EditorConnectionState;
      readonly retryInMilliseconds?: number;
    }
  | {
      readonly kind: "sync-state";
      readonly state: EditorSyncState;
      readonly confirmedSequence: number;
      readonly problemCode?: string;
    }
  | {
      readonly kind: "selection-changed";
      readonly selection: OpaqueBinary;
    }
  | {
      readonly kind: "fatal-error";
      readonly problemCode: string;
    };

export interface EditorMountRequest {
  readonly protocolVersion: EditorBridgeProtocolVersion;
  readonly canvas: HTMLCanvasElement;
  readonly reducedMotion: boolean;
  readonly locale?: string;
  readonly onEvent: (event: EditorEvent) => void;
}

export interface CloudDocumentOpenRequest {
  readonly protocolVersion: EditorBridgeProtocolVersion;
  readonly documentId: CloudDocumentId;
  readonly realtimeEndpoint: URL;
  readonly sessionTicket: EditorSessionTicket;
  readonly initialSnapshot?: OpaqueBinary;
  readonly resumeAfterSequence?: number;
}

export interface ExportRequest {
  readonly format: "figcraft" | "png" | "svg" | "pdf";
  readonly scale?: number;
}

/** One mounted Rust/WASM editor instance. Calls are serialized by the implementation. */
export interface MountedEditor {
  openCloudDocument(request: CloudDocumentOpenRequest): Promise<void>;
  replaceSessionTicket(ticket: EditorSessionTicket): Promise<void>;
  exportDocument(request: ExportRequest): Promise<OpaqueBinary>;
  closeDocument(): Promise<void>;
  unmount(): Promise<void>;
}

/** Entry point implemented by the generated Rust/WASM adapter. */
export interface EditorBridge {
  readonly protocolVersion: EditorBridgeProtocolVersion;
  mount(request: EditorMountRequest): Promise<MountedEditor>;
}
