import { createContext, useContext, useMemo, useReducer, type ReactNode } from 'react'
import type { PanelType } from '../command/registry'

export interface PanelInstance {
  id: string
  type: PanelType
  symbol?: string
  title: string
}

interface WorkspaceState {
  panels: PanelInstance[]
  focusId: string | null
}

type Action =
  | { type: 'open'; panel: Omit<PanelInstance, 'id'> }
  | { type: 'close'; id: string }
  | { type: 'focus'; id: string }

function reducer(state: WorkspaceState, action: Action): WorkspaceState {
  switch (action.type) {
    case 'open': {
      // De-duplicate: opening the same type+symbol focuses the existing panel.
      const existing = state.panels.find(
        (p) => p.type === action.panel.type && p.symbol === action.panel.symbol,
      )
      if (existing) return { ...state, focusId: existing.id }
      const id = `${action.panel.type}-${action.panel.symbol ?? 'x'}-${Date.now()}`
      return { panels: [...state.panels, { ...action.panel, id }], focusId: id }
    }
    case 'close': {
      const panels = state.panels.filter((p) => p.id !== action.id)
      const focusId =
        state.focusId === action.id
          ? panels.length > 0
            ? panels[panels.length - 1].id
            : null
          : state.focusId
      return { panels, focusId }
    }
    case 'focus':
      return { ...state, focusId: action.id }
    default:
      return state
  }
}

interface WorkspaceApi extends WorkspaceState {
  open: (panel: Omit<PanelInstance, 'id'>) => void
  close: (id: string) => void
  focus: (id: string) => void
}

const WorkspaceContext = createContext<WorkspaceApi | null>(null)

export function WorkspaceProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, { panels: [], focusId: null })

  const api = useMemo<WorkspaceApi>(
    () => ({
      ...state,
      open: (panel) => dispatch({ type: 'open', panel }),
      close: (id) => dispatch({ type: 'close', id }),
      focus: (id) => dispatch({ type: 'focus', id }),
    }),
    [state],
  )

  return <WorkspaceContext.Provider value={api}>{children}</WorkspaceContext.Provider>
}

export function useWorkspace(): WorkspaceApi {
  const ctx = useContext(WorkspaceContext)
  if (!ctx) throw new Error('useWorkspace must be used within <WorkspaceProvider>')
  return ctx
}
