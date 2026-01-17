/**
 * Migration system for running one-time data migrations
 *
 * Each migration has a unique name and version. Migrations run once per version.
 * To re-run a migration, bump its version number.
 */

const MIGRATIONS_KEY = 'hashtree:migrations';

interface MigrationRecord {
  [name: string]: number; // name -> version that was run
}

interface Migration {
  name: string;
  version: number;
  run: (npub: string) => Promise<void>;
}

const migrations: Migration[] = [];

function getMigrationRecords(): MigrationRecord {
  try {
    const stored = localStorage.getItem(MIGRATIONS_KEY);
    return stored ? JSON.parse(stored) : {};
  } catch {
    return {};
  }
}

function setMigrationRecords(records: MigrationRecord): void {
  localStorage.setItem(MIGRATIONS_KEY, JSON.stringify(records));
}

/**
 * Register a migration to be run
 */
export function registerMigration(name: string, version: number, run: (npub: string) => Promise<void>): void {
  migrations.push({ name, version, run });
}

/**
 * Check if any migrations need to run
 */
export function needsMigrations(): boolean {
  const records = getMigrationRecords();
  return migrations.some(m => (records[m.name] || 0) < m.version);
}

/**
 * Run all pending migrations
 */
export async function runMigrations(npub: string): Promise<void> {
  const records = getMigrationRecords();

  for (const migration of migrations) {
    const lastRun = records[migration.name] || 0;
    if (lastRun < migration.version) {
      console.log(`[Migrations] Running ${migration.name} v${migration.version}...`);
      try {
        await migration.run(npub);
        records[migration.name] = migration.version;
        setMigrationRecords(records);
        console.log(`[Migrations] Completed ${migration.name}`);
      } catch (e) {
        console.error(`[Migrations] Failed ${migration.name}:`, e);
      }
    }
  }
}

// Register video metadata migration
import { runVideoMetadataMigration } from './videoMetadata';

registerMigration('video-metadata', 6, runVideoMetadataMigration);

// Register re-encryption migration for unencrypted trees
import { runReencryptMigration } from './reencrypt';

registerMigration('reencrypt-trees', 1, runReencryptMigration);
