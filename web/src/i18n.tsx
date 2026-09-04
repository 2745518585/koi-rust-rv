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
    workspace: "工作空间",
    overview: "总览",
    tasks: "任务队列",
    approvals: "待处理审批",
    tools: "工具目录",
    audit: "事件审计",
    connected: "API 已连接",
    disconnected: "正在连接 API",
    live: "实时",
    userWorkspace: "用户工作空间",
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
    workspace: "WORKSPACE",
    overview: "Overview",
    tasks: "Task queue",
    approvals: "Approvals",
    tools: "Tool catalog",
    audit: "Event audit",
    connected: "API connected",
    disconnected: "Connecting API",
    live: "Live",
    userWorkspace: "User workspace",
  },
} as const;

type MessageKey = keyof (typeof messages)["zh-CN"];
type I18n = { locale: Locale; setLocale: (locale: Locale) => void; t: (key: MessageKey) => string };
const I18nContext = createContext<I18n | null>(null);

export function I18nProvider({ children }: { children: React.ReactNode }) {
  const [locale, setLocale] = useState<Locale>(() =>
    window.localStorage.getItem("koi.locale") === "en" ? "en" : "zh-CN",
  );
  useEffect(() => {
    document.documentElement.lang = locale;
    window.localStorage.setItem("koi.locale", locale);
  }, [locale]);
  const value = useMemo<I18n>(() => ({ locale, setLocale, t: (key) => messages[locale][key] }), [locale]);
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18n {
  const context = useContext(I18nContext);
  if (!context) throw new Error("I18nProvider is required");
  return context;
}
