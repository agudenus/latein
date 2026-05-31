import type { ReactNode } from 'react'
import { useWorkspace, type PanelInstance } from '../workspace/workspaceStore'

export function Panel({ panel, children }: { panel: PanelInstance; children: ReactNode }) {
  const { close, focus, focusId } = useWorkspace()
  const focused = focusId === panel.id

  return (
    <section
      onMouseDown={() => focus(panel.id)}
      className={`flex min-h-0 flex-col overflow-hidden rounded border bg-terminal-panel ${
        focused ? 'border-terminal-accent' : 'border-terminal-border'
      }`}
    >
      <div className="flex items-center justify-between border-b border-terminal-border px-2 py-1">
        <span className="truncate text-xs font-semibold tracking-wide text-terminal-text">
          {panel.title}
        </span>
        <button
          onClick={() => close(panel.id)}
          aria-label="Panel schließen"
          className="px-1 text-terminal-muted hover:text-terminal-down"
        >
          ✕
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">{children}</div>
    </section>
  )
}
