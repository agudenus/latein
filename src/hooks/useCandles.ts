import { useEffect, useState } from 'react'
import type { Candle, Range } from '../types/market'
import { useProvider } from '../data/DataProvider'

export function useCandles(symbol: string | undefined, range: Range): Candle[] {
  const provider = useProvider()
  const [candles, setCandles] = useState<Candle[]>([])

  useEffect(() => {
    setCandles(symbol ? provider.getCandles(symbol, range) : [])
  }, [provider, symbol, range])

  return candles
}
