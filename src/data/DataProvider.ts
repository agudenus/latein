import { createContext, useContext } from 'react'
import type { Security, Quote, Candle, NewsItem, Range } from '../types/market'

/**
 * Abstraction over a market-data source. The v1 implementation is a fully
 * client-side mock ({@link MockProvider}); swapping in a real API later only
 * requires another implementation of this interface.
 */
export interface DataProvider {
  search(query: string): Security[]
  getSecurity(symbol: string): Security | undefined
  /** Subscribe to live quotes. Returns an unsubscribe function. */
  subscribeQuote(symbol: string, cb: (q: Quote) => void): () => void
  getCandles(symbol: string, range: Range): Candle[]
  getNews(symbol?: string): NewsItem[]
}

export const ProviderContext = createContext<DataProvider | null>(null)

export function useProvider(): DataProvider {
  const provider = useContext(ProviderContext)
  if (!provider) {
    throw new Error('useProvider must be used within a <ProviderContext.Provider>')
  }
  return provider
}
