import { useWorkspace, type PanelInstance } from '../workspace/workspaceStore'
import { Panel } from './Panel'
import { QuotePanel } from './panels/QuotePanel'
import { ChartPanel } from './panels/ChartPanel'
import { NewsPanel } from './panels/NewsPanel'
import { WatchlistPanel } from './panels/WatchlistPanel'
import { HelpPanel } from './panels/HelpPanel'

export function Workspace() {
  const { panels } = useWorkspace()

  if (panels.length === 0) return <EmptyState />

  return (
    <main
      className="grid flex-1 gap-2 overflow-auto p-2"
      style={{
        gridTemplateColumns: 'repeat(auto-fill, minmax(360px, 1fr))',
        gridAutoRows: 'minmax(280px, auto)',
      }}
    >
      {panels.map((p) => (
        <Panel key={p.id} panel={p}>
          <PanelBody panel={p} />
        </Panel>
      ))}
    </main>
  )
}

function PanelBody({ panel }: { panel: PanelInstance }) {
  switch (panel.type) {
    case 'quote':
      return <QuotePanel symbol={panel.symbol!} />
    case 'chart':
      return <ChartPanel symbol={panel.symbol!} />
    case 'news':
      return <NewsPanel symbol={panel.symbol} />
    case 'watchlist':
      return <WatchlistPanel />
    case 'help':
      return <HelpPanel />
    default:
      return null
  }
}

function EmptyState() {
  return (
    <div className="flex flex-1 items-center justify-center text-center text-terminal-muted">
      <div>
        <div className="text-lg font-bold tracking-widest text-terminal-accent">LATEIN TERMINAL</div>
        <p className="mt-2 text-sm">
          Gib einen Befehl ein — z.B. <span className="text-terminal-text">AAPL GP</span> oder{' '}
          <span className="text-terminal-text">HELP</span>.
        </p>
      </div>
    </div>
  )
}
