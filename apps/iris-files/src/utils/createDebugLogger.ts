/* eslint-disable @typescript-eslint/no-explicit-any */
import debug from "debug"

export interface DebugLogger {
  log: (...args: any[]) => void
  warn: (...args: any[]) => void
  error: (...args: any[]) => void
}

/**
 * Create debug loggers for a given namespace
 * Usage: const {log, warn, error} = createDebugLogger('ndk:relay')
 */
export function createDebugLogger(namespace: string): DebugLogger {
  const log = debug(namespace)
  const warn = debug(`${namespace}:warn`)
  const err = debug(`${namespace}:error`)

  return {
    log: (...args: any[]) => {
      ;(log as any)(...args)
    },
    warn: (...args: any[]) => {
      ;(warn as any)(...args)
    },
    error: (...args: any[]) => {
      ;(err as any)(...args)
    },
  }
}
