import { invoke, isTauri } from '@tauri-apps/api/core';
import { managementApi, readString } from './managementApi';
import { authFileName, parseAuthFilePriority } from './authFiles';
import type { AuthFile } from './quotaService';

/**
 * Credential order of one provider family: rows of credential file names, top row first.
 * Routing uses the top row until all of its credentials are unavailable; the credentials
 * of one row share the traffic (the core's round-robin within one priority).
 */
export type FamilyOrder = string[][];

/** A named set of family orders the user switches between. */
export type RoutingProfile = { name: string; families: Record<string, FamilyOrder> };

export type RoutingProfiles = { active: string; profiles: RoutingProfile[] };

/** The family a credential is ordered in: its provider (claude, kimi, kiro4cpa, ...). */
export const familyOf = (file: AuthFile): string =>
  readString(file, 'provider', 'type', 'account_type').toLowerCase();

/** The live order of each family, read from the credentials' priorities (higher first). */
export const liveOrders = (files: AuthFile[]): Record<string, FamilyOrder> => {
  const byFamily = new Map<string, Map<number, string[]>>();
  files.forEach((file) => {
    const rows = byFamily.get(familyOf(file)) ?? new Map<number, string[]>();
    const priority = parseAuthFilePriority(file.priority) ?? 0;
    rows.set(priority, [...(rows.get(priority) ?? []), authFileName(file)]);
    byFamily.set(familyOf(file), rows);
  });
  return Object.fromEntries([...byFamily].map(([family, rows]) => [family,
    [...rows].sort(([left], [right]) => right - left).map(([, names]) => names.sort())]));
};

/**
 * The order to apply for a family: the stored rows restricted to existing credentials,
 * with credentials the order does not know yet in a last row.
 */
export const resolveOrder = (order: FamilyOrder, names: string[]): FamilyOrder => {
  const known = new Set(names);
  const rows = order.map((row) => row.filter((name) => known.has(name))).filter((row) => row.length);
  const placed = new Set(rows.flat());
  const rest = names.filter((name) => !placed.has(name));
  return rest.length ? [...rows, rest] : rows;
};

/** Move a credential into row `row` of the order, or into a new row before index `row` when `newRow`. */
export const moveCredential = (order: FamilyOrder, name: string, row: number, newRow: boolean): FamilyOrder => {
  const rows = order.map((items) => items.filter((item) => item !== name));
  if (newRow) rows.splice(row, 0, [name]);
  else rows[row] = [...rows[row], name];
  return rows.filter((items) => items.length);
};

/** Write a family order as priorities: the top of n rows gets 10n, the last 10. Unchanged credentials are skipped. */
export const applyOrder = async (files: AuthFile[], order: FamilyOrder) => {
  const current = new Map(files.map((file) => [authFileName(file), parseAuthFilePriority(file.priority) ?? 0]));
  const changes = order.flatMap((row, index) => row.map((name) => [name, (order.length - index) * 10] as const))
    .filter(([name, priority]) => current.has(name) && current.get(name) !== priority);
  await Promise.all(changes.map(([name, priority]) => managementApi.patch('/credentials/fields', { name, priority })));
};

const isProfiles = (value: unknown): value is RoutingProfiles =>
  typeof value === 'object' && value !== null && Array.isArray((value as RoutingProfiles).profiles)
  && typeof (value as RoutingProfiles).active === 'string';

const STORAGE_KEY = 'cpa-gui.routing-profiles.v1';

/** Stored profiles (app directory, or browser storage outside the desktop app), or null when none were saved yet. */
export const loadRoutingProfiles = async (): Promise<RoutingProfiles | null> => {
  let value: unknown = null;
  if (isTauri()) value = await invoke('get_routing_profiles');
  else try { value = JSON.parse(window.localStorage.getItem(STORAGE_KEY) ?? 'null'); } catch { value = null; }
  return isProfiles(value) ? value : null;
};

export const saveRoutingProfiles = async (profiles: RoutingProfiles) => {
  if (isTauri()) await invoke('save_routing_profiles', { profiles });
  else try { window.localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles)); } catch { /* Keep the session state. */ }
};
