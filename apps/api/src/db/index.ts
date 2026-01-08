import { drizzle } from 'drizzle-orm/better-sqlite3';
import Database from 'better-sqlite3';
import * as schema from './schema';
import { join } from 'path';

// Use a data directory for the DB file
const dbPath = process.env.DB_PATH || join(process.cwd(), 'data/sqlite.db');

// Ensure directory exists - logic handled by mkdir if needed, 
// but better-sqlite3 throws if dir doesn't exist usually, or creates file if dir exists.
// For now, assuming data dir exists or is created by tokenStore legacy logic.
// We can improve this initialization later.

const sqlite = new Database(dbPath);
export const db = drizzle(sqlite, { schema });
