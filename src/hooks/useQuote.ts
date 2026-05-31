import { useEffect, useState } from 'react'
import type { Quote } from '../types/market'
import { useProvider } from '../data/DataProvider'

/** Subscribe to live quotes for a symbol. Returns null until the first tick. */
export function useQuote(symbol: string | undefined): Quote | null {
  const provider = useProvider()
  const [quote, setQuote] = useState<Quote | null>(null)

  useEffect(() => {
    if (!symbol) {
      setQuote(null)
      return
    }
    const unsubscribe = provider.subscribeQuote(symbol, setQuote)
    return unsubscribe
  }, [provider, symbol])

  return quote
}
