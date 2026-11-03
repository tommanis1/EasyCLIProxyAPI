import { invoke } from '@tauri-apps/api/core';

export type CoreConfigSettings = {
  managementSecretConfigured: boolean;
  apiKeys: CoreApiKey[];
  debug: boolean;
  commercialMode: boolean;
  loggingToFile: boolean;
  logsMaxTotalSizeMb: number;
  errorLogsMaxFiles: number;
  usageStatisticsEnabled: boolean;
  redisUsageQueueRetentionSeconds: number;
  host: string;
  port: number;
  allowLan: boolean;
  routingStrategy: string;
  proxyUrl: string;
  proxyOverride: boolean;
  routingSessionAffinity: boolean;
  routingSessionAffinityTtl: string;
  disableCooling: boolean;
  requestRetry: number;
  maxRetryCredentials: number;
  maxRetryInterval: number;
  streamingBootstrapRetries: number;
};

export type CoreApiKey = {
  apiKey: string;
  remark: string;
};

export type CoreTlsSettings = {
  enabled: boolean;
  cert: string;
  key: string;
};

export const getCoreConfigSettings = (): Promise<CoreConfigSettings> =>
  invoke<CoreConfigSettings>('get_core_config_settings');

export const getCoreTlsSettings = (): Promise<CoreTlsSettings> =>
  invoke<CoreTlsSettings>('get_core_tls_settings');

export const updateCoreApiKey = (args: { originalApiKey: string; apiKey: string; remark: string; }): Promise<CoreConfigSettings> =>
  invoke<CoreConfigSettings>('update_core_api_key', args);

export const addCoreApiKey = (args: { apiKey: string; remark: string; }): Promise<CoreConfigSettings> =>
  invoke<CoreConfigSettings>('add_core_api_key', args);

export const openCoreLogsDirectory = (): Promise<void> =>
  invoke<void>('open_core_logs_directory');

export const deleteCoreApiKey = (args: { apiKey: string; }): Promise<CoreConfigSettings> =>
  invoke<CoreConfigSettings>('delete_core_api_key', args);
