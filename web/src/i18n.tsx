import { createContext, useContext, useEffect, useMemo, useState } from "react";

export type Locale = "zh-CN" | "en";

const messages = {
  "zh-CN": {
    language: "EN",
    loadingSession: "正在恢复安全会话…",
    login: "登录",
    register: "注册并登录",
    loginTitle: "登录控制台",
    registerTitle: "创建你的账户",
    email: "邮箱",
    username: "用户名",
    password: "密码",
    logout: "退出登录",
    authCopyLogin: "使用邮箱和密码继续。",
    authCopyRegister: "用户名将作为写入 Koi 核心事件的稳定用户标识。",
    authSwitchToRegister: "还没有账户？注册",
    authSwitchToLogin: "已有账户？登录",

    newSession: "新建会话",
    mainSession: "主会话",
    taskSessions: "任务会话",
    noSessions: "还没有会话。",
    mainNotice: "主会话需要授权后才会显示。",
    approvals: "审批",
    tools: "工具",
    audit: "审计",
    connected: "已连接",
    disconnected: "连接中",

    modeConversation: "对话",
    modeEvents: "事件",
    pause: "暂停",
    resume: "恢复",
    stop: "中止",
    rename: "重命名",
    remove: "删除",
    refresh: "刷新",
    typePlaceholder: "输入要交给 Agent 的内容…",
    send: "发送",
    sending: "发送中…",
    suggestedPermission: "建议授权",
    suggestedPermissionHint: "本次 Web 操作携带的权限上限建议，服务端仍会重新核定。",
    enterHint: "Enter 发送 · Shift+Enter 换行",
    loadingFeed: "正在读取会话…",
    noMessages: "还没有对话内容。",
    allEvents: "全部事件",
    defaultModel: "默认模型",
    noTaskTitle: "还没有会话",
    noTaskHint: "创建一个会话，把现场交给 Agent。",

    refreshed: "数据已刷新",
    refreshFailed: "刷新失败，请检查 API 服务",
    approvalSubmitted: "授权已提交，任务将继续运行",
    approvalDenied: "已拒绝此次操作",
    approvalFailed: "审批提交失败，请稍后重试",
    backendOffline: "后端未连接，无法执行该操作",
    taskCreated: "会话已创建",
    taskCreateFailed: "会话创建失败",
    renamed: "会话已重命名",
    deleted: "会话已删除",
    controlFailed: "操作未能完成",
    inputFailed: "消息发送失败",
    requestStopReason: "Web 用户请求中止该任务",
    renamePrompt: "输入新的会话名称",
    deleteConfirm: "确定删除该会话？",
    disconnectedTitle: "控制台未连接到可用后端",
    connBroken: "无法读取后端数据：请检查服务日志、登录权限或事件存储。",
    retry: "重新连接",
    elevationEyebrow: "提权请求",
    elevationTitle: "Agent 需要你的确认",
    elevationCopy: "该工具操作正在等待授权。请在确认目标和参数后决定是否放行。",
    elevationTool: "请求的工具",
    elevationSession: "会话",
    elevationScope: "作用域",
    elevationRequested: "请求时间",
    elevationArguments: "请求参数",
    elevationLoading: "正在从安全事件记录读取审批详情…",
    elevationNotice: "批准或拒绝都会写入审计事件；暂缓不会放行操作。",
    elevationDefer: "稍后处理",
    elevationDeny: "拒绝",
    elevationApprove: "批准操作",
    elevationQueue: "另有 {{count}} 个提权请求等待处理",
  },
  en: {
    language: "中文",
    loadingSession: "Restoring secure session…",
    login: "Sign in",
    register: "Create account",
    loginTitle: "Sign in to console",
    registerTitle: "Create your account",
    email: "Email",
    username: "Username",
    password: "Password",
    logout: "Sign out",
    authCopyLogin: "Continue with your email and password.",
    authCopyRegister: "Your username is the stable identity recorded in Koi core events.",
    authSwitchToRegister: "No account? Register",
    authSwitchToLogin: "Have an account? Sign in",

    newSession: "New session",
    mainSession: "Main session",
    taskSessions: "Task sessions",
    noSessions: "No sessions yet.",
    mainNotice: "The main session appears only for authorized accounts.",
    approvals: "Approvals",
    tools: "Tools",
    audit: "Audit",
    connected: "Connected",
    disconnected: "Connecting",

    modeConversation: "Chat",
    modeEvents: "Events",
    pause: "Pause",
    resume: "Resume",
    stop: "Interrupt",
    rename: "Rename",
    remove: "Delete",
    refresh: "Refresh",
    typePlaceholder: "Message the agent…",
    send: "Send",
    sending: "Sending…",
    suggestedPermission: "Suggested permission",
    suggestedPermissionHint: "Permission ceiling suggested for this web action; the server re-validates it.",
    enterHint: "Enter to send · Shift+Enter for newline",
    loadingFeed: "Loading session…",
    noMessages: "No messages yet.",
    allEvents: "All events",
    defaultModel: "Default model",
    noTaskTitle: "No sessions yet",
    noTaskHint: "Create a session to brief the agent.",

    refreshed: "Data refreshed",
    refreshFailed: "Refresh failed, check the API service",
    approvalSubmitted: "Approval submitted, the task will continue",
    approvalDenied: "Operation denied",
    approvalFailed: "Failed to submit approval, try again later",
    backendOffline: "Backend not connected",
    taskCreated: "Session created",
    taskCreateFailed: "Failed to create session",
    renamed: "Session renamed",
    deleted: "Session deleted",
    controlFailed: "Action could not be completed",
    inputFailed: "Failed to send message",
    requestStopReason: "Web user requested task interruption",
    renamePrompt: "Enter a new session name",
    deleteConfirm: "Delete this session?",
    disconnectedTitle: "Console is not connected to a backend",
    connBroken: "Cannot read backend data: check service logs, permissions or the event store.",
    retry: "Reconnect",
    elevationEyebrow: "Authorization request",
    elevationTitle: "The agent needs your confirmation",
    elevationCopy: "This tool operation is waiting for authorization. Review its target and arguments before deciding.",
    elevationTool: "Requested tool",
    elevationSession: "Session",
    elevationScope: "Scope",
    elevationRequested: "Requested",
    elevationArguments: "Arguments",
    elevationLoading: "Reading approval details from the secure event record…",
    elevationNotice: "Approving or denying writes an audit event; deferring does not allow the operation.",
    elevationDefer: "Review later",
    elevationDeny: "Deny",
    elevationApprove: "Approve operation",
    elevationQueue: "{{count}} more authorization requests are waiting",
  },
} as const;

export type MessageKey = keyof (typeof messages)["zh-CN"];

type I18n = {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  t: (key: MessageKey, variables?: Record<string, string | number>) => string;
};
const I18nContext = createContext<I18n | null>(null);

export function I18nProvider({ children }: { children: React.ReactNode }) {
  const [locale, setLocale] = useState<Locale>(() =>
    window.localStorage.getItem("koi.locale") === "en" ? "en" : "zh-CN",
  );
  useEffect(() => {
    document.documentElement.lang = locale;
    window.localStorage.setItem("koi.locale", locale);
  }, [locale]);
  const value = useMemo<I18n>(
    () => ({
      locale,
      setLocale,
      t: (key, variables) => {
        const message = messages[locale][key] as string;
        return Object.entries(variables ?? {}).reduce(
          (current, [name, value]) => current.replaceAll(`{{${name}}}`, String(value)),
          message,
        );
      },
    }),
    [locale],
  );
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18n {
  const context = useContext(I18nContext);
  if (!context) throw new Error("I18nProvider is required");
  return context;
}
