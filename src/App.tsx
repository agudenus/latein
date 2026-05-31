import { useEffect, useMemo, useState } from 'react'
import { ProviderContext } from './data/DataProvider'
import { MockProvider } from './data/MockProvider'
import { WorkspaceProvider, useWorkspace } from './workspace/workspaceStore'
import { TickerTape } from './components/TickerTape'
import { CommandBar } from './components/CommandBar'
import { Workspace } from './components/Workspace'

export default function App() {
  const provider = useMemo(() => new MockProvider(), [])

  return (
    <ProviderContext.Provider value={provider}>
      <WorkspaceProvider>
        <Shell />
      </WorkspaceProvider>
    </ProviderContext.Provider>
  )
}

function Shell() {
  const { open } = useWorkspace()

  // Open a couple of default panels on first load.
  useEffect(() => {
    open({ type: 'watchlist', title: 'Watchlist' })
    open({ type: 'help', title: 'Help' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  return (
    <div className="flex h-full flex-col">
      <Header />
      <TickerTape />
      <CommandBar />
      <Workspace />
    </div>
  )
}

function Header() {
  return (
    <header className="flex items-center justify-between border-b border-terminal-border px-3 py-1.5">
      <div className="flex items-baseline gap-2">
        <span className="font-bold tracking-widest text-terminal-accent">LATEIN</span>
        <span className="text-xs text-terminal-muted">TERMINAL</span>
      </div>
      <Clock />
    </header>
  )
}

function Clock() {
  const [now, setNow] = useState(() => new Date())
  useEffect(() => {
    const t = window.setInterval(() => setNow(new Date()), 1000)
    return () => window.clearInterval(t)
  }, [])
  return (
    <span className="text-xs text-terminal-muted tnum">
      {now.toUTCString().slice(17, 25)} UTC
    </span>
  )
}
