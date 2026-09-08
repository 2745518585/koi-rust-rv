import { useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useI18n } from "../i18n";

// 对话气泡默认保持紧凑；较长内容仍保留完整正文，并允许用户按需展开。
const COLLAPSED_CHARACTER_LIMIT = 1_200;
const COLLAPSED_LINE_LIMIT = 18;

export interface MarkdownContentProps {
  content: string | null | undefined;
  className?: string;
  expandable?: boolean;
}

/**
 * 渲染事件正文使用的安全 Markdown。
 *
 * 不启用 rehypeRaw，因此事件中的 HTML 不会被当作页面结构执行；GFM 只负责
 * 表格、任务列表、删除线等常见 Markdown 扩展。
 */
export function MarkdownContent({
  content,
  className = "",
  expandable = true,
}: MarkdownContentProps) {
  const { t } = useI18n();
  const [expanded, setExpanded] = useState(false);
  const value = content ?? "";
  const shouldOfferExpand =
    expandable &&
    (value.length > COLLAPSED_CHARACTER_LIMIT || value.split(/\r?\n/).length > COLLAPSED_LINE_LIMIT);

  // 事件正文可能在流转结束后被完整结果替换；替换正文时重新从折叠状态开始。
  useEffect(() => {
    setExpanded(false);
  }, [value]);

  if (!value.trim()) {
    return <span className={`markdown-content ${className}`.trim()}>—</span>;
  }

  return (
    <div className={`markdown-content ${className}`.trim()}>
      <div className={`markdown-body ${shouldOfferExpand && !expanded ? "markdown-body-collapsed" : ""}`.trim()}>
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{value}</ReactMarkdown>
      </div>
      {shouldOfferExpand ? (
        <button
          className="markdown-toggle"
          type="button"
          aria-expanded={expanded}
          onClick={() => setExpanded((current) => !current)}
        >
          {expanded ? t("collapseContent") : t("expandContent")}
        </button>
      ) : null}
    </div>
  );
}
