// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error'

export interface LogLevelResponse {
  level:  LogLevel
  levels: LogLevel[]
}

export const logsApi = {
  getLevel(): Promise<LogLevelResponse> {
    return api.get<LogLevelResponse>('/api/logs/level').then((r) => r.data)
  },
  setLevel(level: LogLevel): Promise<LogLevelResponse> {
    return api.put<LogLevelResponse>('/api/logs/level', { level }).then((r) => r.data)
  },
}
