// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  accountIdHash?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'api_key'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

export interface KiroCliOAuthLoginRequest {
  cliPath?: string
  license?: 'free' | 'pro'
  identityProvider?: string
  region?: string
  dbPath?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  forceLogout?: boolean
  useDeviceFlow?: boolean
}

export interface KiroCliOAuthCallbackRequest {
  callbackUrl: string
}

export interface KiroCliOAuthLoginStatus {
  running: boolean
  phase: string
  output: string[]
  loginUrl: string | null
  startedAt: string | null
  finishedAt: string | null
  exitCode: number | null
  importedCredentialId: number | null
  error: string | null
}

export interface KiroCliOAuthLoginResponse {
  success: boolean
  message: string
  status: KiroCliOAuthLoginStatus
}
