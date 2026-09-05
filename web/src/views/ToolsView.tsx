import { useMemo, useState } from "react";
import { Search, Wrench } from "lucide-react";
import type { ToolDefinition, ToolSideEffect } from "../api/types";
import { useI18n } from "../i18n";
import { EmptyState, EffectBadge, labelFor, PanelHeader, PermissionBadge, sideEffectMeta } from "../lib/ui";

export function ToolsView({ tools, onMenu }: { tools: ToolDefinition[]; onMenu: () => void }) {
  const { locale } = useI18n();
  const copy = locale === "en"
    ? {
        title: "Tool catalog",
        subtitle: "Definitions come from the Rust core registry; the web only shows risk and permission boundaries.",
        search: "Search tools",
        all: "All side effects",
        registered: (n: number) => `${n} registered tools`,
        failClosed: "Policy defaults to fail-closed",
        empty: "No matching tools",
        emptyHint: "Try another keyword or side-effect filter.",
        visible: "model visible",
        hidden: "model hidden",
        timeout: (s: number) => `${s}s timeout`,
      }
    : {
        title: "工具目录",
        subtitle: "工具定义来自 Rust 核心注册表，Web 端只负责展示风险与权限边界。",
        search: "搜索工具名称或说明",
        all: "全部副作用",
        registered: (n: number) => `已注册 ${n} 个工具`,
        failClosed: "策略默认 fail-closed",
        empty: "没有匹配的工具",
        emptyHint: "换个关键词或副作用筛选条件。",
        visible: "模型可见",
        hidden: "模型隐藏",
        timeout: (s: number) => `超时 ${s}s`,
      };

  const [query, setQuery] = useState("");
  const [effect, setEffect] = useState<ToolSideEffect | "all">("all");

  const effects: Array<ToolSideEffect | "all"> = useMemo(
    () => ["all", ...(Object.keys(sideEffectMeta) as ToolSideEffect[])],
    [],
  );

  const visible = tools.filter((tool) => {
    const matchesQuery =
      !query.trim() || `${tool.name} ${tool.description}`.toLowerCase().includes(query.toLowerCase());
    return matchesQuery && (effect === "all" || tool.sideEffect === effect);
  });

  return (
    <section className="panel">
      <PanelHeader
        title={copy.title}
        description={copy.subtitle}
        onMenu={onMenu}
        actions={
          <span className="panel-note">
            <strong>{copy.registered(tools.length)}</strong>
            <span className="badge effect-readonly">{copy.failClosed}</span>
          </span>
        }
      />
      <div className="panel-body">
        <div className="panel-card">
          <div className="filter-row">
            <div className="search-field">
              <Search size={15} />
              <input
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={copy.search}
                aria-label={copy.search}
              />
            </div>
            <div className="chip-row">
              {effects.map((value) => (
                <button
                  key={value}
                  className={`chip ${effect === value ? "chip-active" : ""}`}
                  onClick={() => setEffect(value)}
                >
                  {value === "all" ? copy.all : labelFor(sideEffectMeta[value].label, locale)}
                </button>
              ))}
            </div>
          </div>
          {visible.length ? (
            <div className="tool-list">
              {visible.map((tool) => (
                <article
                  key={tool.name}
                  className={`tool-row ${tool.sideEffect === "Destructive" ? "tool-row-risk" : ""}`}
                >
                  <span className="tool-row-name">{tool.name}</span>
                  <p className="tool-row-desc">{tool.description}</p>
                  <div className="tool-row-meta">
                    <PermissionBadge level={tool.requiredPermission} />
                    <EffectBadge effect={tool.sideEffect} />
                    <span>{copy.timeout(tool.timeoutMs / 1000)}</span>
                    <span className={tool.modelVisible ? "tool-visible" : "tool-hidden"}>
                      {tool.modelVisible ? copy.visible : copy.hidden}
                    </span>
                  </div>
                </article>
              ))}
            </div>
          ) : (
            <EmptyState icon={Wrench} title={copy.empty} description={copy.emptyHint} />
          )}
        </div>
      </div>
    </section>
  );
}
