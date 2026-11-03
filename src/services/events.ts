import { listen } from '@tauri-apps/api/event';
import type { CoreStatus } from './coreLifecycle';

type Handler<T> = (event: { payload: T }) => void;
export const onCoreStatusChanged = (handler: Handler<CoreStatus>): Promise<() => void> => listen('core-status-changed', handler);
export const onConfigFilesChanged = (handler: () => void): Promise<() => void> => listen('config-files-changed', handler);
export const onUsageRecordsUpdated = (handler: () => void): Promise<() => void> => listen('usage-records-updated', handler);
export const onWindowsCloseRequested = (handler: () => void): Promise<() => void> => listen('windows-close-requested', handler);
