import type {
  ApprovalRequest,
  ApprovalSubmission,
  CreateTaskRequest,
  StreamEvent,
  SystemSnapshot,
  TaskControlRequest,
  TaskEvent,
  TaskSummary,
} from "./types";

const configuredBase = import.meta.env.VITE_KOI_API_BASE ?? "/api/v1";

function normalizeBase(base: string): string {
  return base.replace(/\/+$/, "");
}

function unwrap<T>(payload: unknown): T {
  if (payload && typeof payload === "object" && "data" in payload) {
    return (payload as { data: T }).data;
  }
  return payload as T;
}

export class KoiApiError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "KoiApiError";
    this.status = status;
  }
}

export type StreamMessageHandler = (event: StreamEvent) => void;

export interface AuthUser {
  userId: string;
  username: string;
  email: string;
  permission: string;
}

interface AuthSessionResponse {
  user: AuthUser;
}

export class KoiApiClient {
  private readonly baseUrl: string;
  private token: string | undefined;

  constructor(baseUrl = configuredBase) {
    this.baseUrl = normalizeBase(baseUrl);
  }

  hasToken(): boolean {
    return Boolean(this.token);
  }

  setToken(token: string): void {
    this.token = token.trim() || undefined;
  }

  async establishSession(): Promise<void> {
    if (!this.token) {
      throw new KoiApiError("请输入 Web 管理访问令牌", 401);
    }
    await this.request<void>("/session", {
      method: "POST",
      headers: { Authorization: `Bearer ${this.token}` },
    });
  }

  async register(input: { email: string; username: string; password: string }): Promise<AuthUser> {
    const session = await this.request<AuthSessionResponse>("/auth/register", {
      method: "POST",
      body: JSON.stringify(input),
    });
    return session.user;
  }

  async login(input: { email: string; password: string }): Promise<AuthUser> {
    const session = await this.request<AuthSessionResponse>("/auth/login", {
      method: "POST",
      body: JSON.stringify(input),
    });
    return session.user;
  }

  async currentUser(): Promise<AuthUser> {
    const session = await this.request<AuthSessionResponse>("/auth/me");
    return session.user;
  }

  async logout(): Promise<void> {
    await this.request<void>("/auth/logout", { method: "POST" });
    this.token = undefined;
  }

  async getSnapshot(signal?: AbortSignal): Promise<SystemSnapshot> {
    return this.request<SystemSnapshot>("/dashboard", { signal });
  }

  async getTaskEvents(taskId: string, signal?: AbortSignal): Promise<TaskEvent[]> {
    return this.request<TaskEvent[]>(`/tasks/${encodeURIComponent(taskId)}/events`, { signal });
  }

  async createTask(request: CreateTaskRequest): Promise<TaskSummary> {
    return this.request<TaskSummary>("/tasks", {
      method: "POST",
      body: JSON.stringify(request),
    });
  }

  async appendTaskContext(
    taskId: string,
    request: { message: string; kind?: "user_message" | "alert" },
  ): Promise<TaskEvent> {
    return this.request<TaskEvent>(`/tasks/${encodeURIComponent(taskId)}/events`, {
      method: "POST",
      body: JSON.stringify({ kind: "user_message", ...request }),
    });
  }

  async requestCancellation(taskId: string, reason: string): Promise<TaskEvent> {
    return this.request<TaskEvent>(`/tasks/${encodeURIComponent(taskId)}/cancellation-requests`, {
      method: "POST",
      body: JSON.stringify({ reason }),
    });
  }

  async controlTask(taskId: string, request: TaskControlRequest): Promise<TaskSummary> {
    return this.request<TaskSummary>(`/tasks/${encodeURIComponent(taskId)}/controls`, {
      method: "POST",
      body: JSON.stringify(request),
    });
  }

  async nameTask(taskId: string, name: string): Promise<TaskSummary> {
    return this.request<TaskSummary>(`/tasks/${encodeURIComponent(taskId)}/name`, {
      method: "POST",
      body: JSON.stringify({ name }),
    });
  }

  async deleteTask(taskId: string): Promise<void> {
    await this.request<void>(`/tasks/${encodeURIComponent(taskId)}`, { method: "DELETE" });
  }

  async submitApproval(
    approvalRequestEventId: string,
    submission: ApprovalSubmission,
  ): Promise<ApprovalRequest> {
    return this.request<ApprovalRequest>(
      `/approvals/${encodeURIComponent(approvalRequestEventId)}`,
      {
        method: "POST",
        body: JSON.stringify(submission),
      },
    );
  }

  openEventStream(
    taskId: string | undefined,
    onMessage: StreamMessageHandler,
    onError?: () => void,
  ): () => void {
    const streamUrl = new URL(`${this.baseUrl}/events/stream`, window.location.origin);
    if (taskId) streamUrl.searchParams.set("task_id", taskId);
    const source = new EventSource(streamUrl.toString(), { withCredentials: true });

    const handleMessage = (message: MessageEvent<string>) => {
      try {
        onMessage(JSON.parse(message.data) as StreamEvent);
      } catch {
        onError?.();
      }
    };

    source.onmessage = handleMessage;
    source.addEventListener("koi.event", handleMessage as EventListener);
    source.onerror = () => onError?.();

    return () => source.close();
  }

  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const response = await fetch(`${this.baseUrl}${path}`, {
      ...init,
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
        ...init.headers,
      },
      credentials: "same-origin",
    });

    if (!response.ok) {
      let detail = "请求未成功";
      try {
        const body = (await response.json()) as { message?: string; error?: string };
        detail = body.message ?? body.error ?? detail;
      } catch {
        // The response may not contain JSON; the HTTP status is enough context here.
      }
      throw new KoiApiError(detail, response.status);
    }

    if (response.status === 204) {
      return undefined as T;
    }

    return unwrap<T>(await response.json());
  }
}

export function createKoiApiClient(): KoiApiClient {
  return new KoiApiClient();
}
