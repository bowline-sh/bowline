import type { DeviceId, EventId, LeaseId, ProjectId, WorkspaceId } from "./ids";
import type { EVENT_SCHEMA_VERSION } from "./ids";
import type { EventName } from "./event-names";
import type { EventWatermarks, StatusScope } from "./status";
import type { CommandOutputBase } from "./commands";

export type EventSeverity = "info" | "attention" | "limited";

export type EventSubjectKind =
  | "workspace"
  | "root"
  | "project"
  | "path"
  | "snapshot"
  | "content"
  | "pack"
  | "policy"
  | "env-record"
  | "setup-receipt"
  | "conflict"
  | "work-view"
  | "lease"
  | "overlay"
  | "device"
  | "metadata"
  | "component";

export type EventSubject = {
  readonly kind: EventSubjectKind;
  readonly id: string;
  readonly path?: string;
};

export type EventActorKind = "system" | "daemon" | "device" | "agent" | "user";

export type EventActor = {
  readonly kind: EventActorKind;
  readonly id?: string;
  readonly displayName?: string;
};

export type EventRedaction = {
  readonly status: "not-needed" | "applied";
  readonly rules?: readonly string[];
};

export type WorkspaceEvent = {
  readonly schemaVersion: typeof EVENT_SCHEMA_VERSION;
  readonly id: EventId;
  readonly name: EventName;
  readonly occurredAt: string;
  readonly severity: EventSeverity;
  readonly summary: string;
  readonly workspaceId: WorkspaceId;
  readonly projectId?: ProjectId;
  readonly path?: string;
  readonly leaseId?: LeaseId;
  readonly deviceId?: DeviceId;
  readonly subject?: EventSubject;
  readonly actor?: EventActor;
  readonly payload?: Record<string, unknown>;
  readonly causationId?: EventId;
  readonly correlationId?: EventId;
  readonly redaction: EventRedaction;
};

export type EventsCommandOutput = CommandOutputBase<"events"> & {
  readonly scope?: StatusScope;
  readonly requestedPath?: string;
  readonly events: readonly WorkspaceEvent[];
  readonly eventWatermarks: EventWatermarks;
};
