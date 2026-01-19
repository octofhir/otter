/**
 * Drizzle ORM compatibility tests for Otter runtime
 * Tests the core drizzle-orm module imports and schema building
 * Note: Full database testing requires actual database drivers
 */

import {
  sql,
  eq,
  ne,
  gt,
  gte,
  lt,
  lte,
  and,
  or,
  not,
  isNull,
  isNotNull,
  inArray,
  notInArray,
  exists,
  notExists,
  between,
  like,
  ilike,
  asc,
  desc,
  count,
  sum,
  avg,
  min,
  max,
  placeholder,
} from 'drizzle-orm';

import {
  pgTable,
  serial,
  text,
  varchar,
  integer,
  boolean,
  timestamp,
  json,
  jsonb,
  uuid,
  primaryKey,
  foreignKey,
  index,
  uniqueIndex,
  numeric,
  real,
  doublePrecision,
  bigint,
  smallint,
  date,
  time,
  interval,
  char,
} from 'drizzle-orm/pg-core';

import {
  mysqlTable,
  int,
  tinyint,
  mediumint,
  bigint as mysqlBigint,
  float,
  double,
  decimal,
  datetime,
  year,
  mysqlEnum,
  text as mysqlText,
  varchar as mysqlVarchar,
  boolean as mysqlBoolean,
  timestamp as mysqlTimestamp,
  json as mysqlJson,
  serial as mysqlSerial,
} from 'drizzle-orm/mysql-core';

import {
  sqliteTable,
  integer as sqliteInteger,
  text as sqliteText,
  real as sqliteReal,
  blob,
} from 'drizzle-orm/sqlite-core';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // SQL Tag and operators
  { name: 'sql tag exists', fn: () => typeof sql, expect: 'function' },
  { name: 'eq operator exists', fn: () => typeof eq, expect: 'function' },
  { name: 'ne operator exists', fn: () => typeof ne, expect: 'function' },
  { name: 'gt operator exists', fn: () => typeof gt, expect: 'function' },
  { name: 'gte operator exists', fn: () => typeof gte, expect: 'function' },
  { name: 'lt operator exists', fn: () => typeof lt, expect: 'function' },
  { name: 'lte operator exists', fn: () => typeof lte, expect: 'function' },
  { name: 'and operator exists', fn: () => typeof and, expect: 'function' },
  { name: 'or operator exists', fn: () => typeof or, expect: 'function' },
  { name: 'not operator exists', fn: () => typeof not, expect: 'function' },
  { name: 'isNull operator exists', fn: () => typeof isNull, expect: 'function' },
  { name: 'isNotNull operator exists', fn: () => typeof isNotNull, expect: 'function' },
  { name: 'inArray operator exists', fn: () => typeof inArray, expect: 'function' },
  { name: 'notInArray operator exists', fn: () => typeof notInArray, expect: 'function' },
  { name: 'exists operator exists', fn: () => typeof exists, expect: 'function' },
  { name: 'notExists operator exists', fn: () => typeof notExists, expect: 'function' },
  { name: 'between operator exists', fn: () => typeof between, expect: 'function' },
  { name: 'like operator exists', fn: () => typeof like, expect: 'function' },
  { name: 'ilike operator exists', fn: () => typeof ilike, expect: 'function' },

  // Order functions
  { name: 'asc function exists', fn: () => typeof asc, expect: 'function' },
  { name: 'desc function exists', fn: () => typeof desc, expect: 'function' },

  // Aggregate functions
  { name: 'count function exists', fn: () => typeof count, expect: 'function' },
  { name: 'sum function exists', fn: () => typeof sum, expect: 'function' },
  { name: 'avg function exists', fn: () => typeof avg, expect: 'function' },
  { name: 'min function exists', fn: () => typeof min, expect: 'function' },
  { name: 'max function exists', fn: () => typeof max, expect: 'function' },

  // Placeholder
  { name: 'placeholder function exists', fn: () => typeof placeholder, expect: 'function' },

  // PostgreSQL table builders
  { name: 'pgTable exists', fn: () => typeof pgTable, expect: 'function' },
  { name: 'serial exists', fn: () => typeof serial, expect: 'function' },
  { name: 'text exists', fn: () => typeof text, expect: 'function' },
  { name: 'varchar exists', fn: () => typeof varchar, expect: 'function' },
  { name: 'integer exists', fn: () => typeof integer, expect: 'function' },
  { name: 'boolean exists', fn: () => typeof boolean, expect: 'function' },
  { name: 'timestamp exists', fn: () => typeof timestamp, expect: 'function' },
  { name: 'json exists', fn: () => typeof json, expect: 'function' },
  { name: 'jsonb exists', fn: () => typeof jsonb, expect: 'function' },
  { name: 'uuid exists', fn: () => typeof uuid, expect: 'function' },
  { name: 'primaryKey exists', fn: () => typeof primaryKey, expect: 'function' },
  { name: 'foreignKey exists', fn: () => typeof foreignKey, expect: 'function' },
  { name: 'index exists', fn: () => typeof index, expect: 'function' },
  { name: 'uniqueIndex exists', fn: () => typeof uniqueIndex, expect: 'function' },
  { name: 'numeric exists', fn: () => typeof numeric, expect: 'function' },
  { name: 'real exists', fn: () => typeof real, expect: 'function' },
  { name: 'doublePrecision exists', fn: () => typeof doublePrecision, expect: 'function' },
  { name: 'bigint exists', fn: () => typeof bigint, expect: 'function' },
  { name: 'smallint exists', fn: () => typeof smallint, expect: 'function' },
  { name: 'date (pg) exists', fn: () => typeof date, expect: 'function' },
  { name: 'time exists', fn: () => typeof time, expect: 'function' },
  { name: 'interval exists', fn: () => typeof interval, expect: 'function' },
  { name: 'char exists', fn: () => typeof char, expect: 'function' },
  // citext is a PostgreSQL extension, may not be exported
  // { name: 'citext exists', fn: () => typeof citext, expect: 'function' },

  // MySQL table builders
  { name: 'mysqlTable exists', fn: () => typeof mysqlTable, expect: 'function' },
  { name: 'int (mysql) exists', fn: () => typeof int, expect: 'function' },
  { name: 'tinyint exists', fn: () => typeof tinyint, expect: 'function' },
  { name: 'mediumint exists', fn: () => typeof mediumint, expect: 'function' },
  { name: 'mysqlBigint exists', fn: () => typeof mysqlBigint, expect: 'function' },
  { name: 'float exists', fn: () => typeof float, expect: 'function' },
  { name: 'double exists', fn: () => typeof double, expect: 'function' },
  { name: 'decimal exists', fn: () => typeof decimal, expect: 'function' },
  { name: 'datetime exists', fn: () => typeof datetime, expect: 'function' },
  { name: 'year exists', fn: () => typeof year, expect: 'function' },
  { name: 'mysqlEnum exists', fn: () => typeof mysqlEnum, expect: 'function' },
  { name: 'mysqlText exists', fn: () => typeof mysqlText, expect: 'function' },
  { name: 'mysqlVarchar exists', fn: () => typeof mysqlVarchar, expect: 'function' },
  { name: 'mysqlBoolean exists', fn: () => typeof mysqlBoolean, expect: 'function' },
  { name: 'mysqlTimestamp exists', fn: () => typeof mysqlTimestamp, expect: 'function' },
  { name: 'mysqlJson exists', fn: () => typeof mysqlJson, expect: 'function' },
  { name: 'mysqlSerial exists', fn: () => typeof mysqlSerial, expect: 'function' },

  // SQLite table builders
  { name: 'sqliteTable exists', fn: () => typeof sqliteTable, expect: 'function' },
  { name: 'sqliteInteger exists', fn: () => typeof sqliteInteger, expect: 'function' },
  { name: 'sqliteText exists', fn: () => typeof sqliteText, expect: 'function' },
  { name: 'sqliteReal exists', fn: () => typeof sqliteReal, expect: 'function' },
  { name: 'blob exists', fn: () => typeof blob, expect: 'function' },

  // Creating a PostgreSQL table schema
  {
    name: 'pgTable creates table',
    fn: () => {
      const users = pgTable('users', {
        id: serial('id').primaryKey(),
        name: text('name').notNull(),
        email: varchar('email', { length: 255 }).unique(),
        age: integer('age'),
        active: boolean('active').default(true),
        createdAt: timestamp('created_at').defaultNow(),
      });
      return typeof users;
    },
    expect: 'object'
  },
  {
    name: 'pgTable schema has columns',
    fn: () => {
      const users = pgTable('users', {
        id: serial('id').primaryKey(),
        name: text('name').notNull(),
      });
      return 'id' in users && 'name' in users;
    },
    expect: true
  },

  // Creating a MySQL table schema
  {
    name: 'mysqlTable creates table',
    fn: () => {
      const products = mysqlTable('products', {
        id: mysqlSerial('id').primaryKey(),
        name: mysqlVarchar('name', { length: 100 }).notNull(),
        price: decimal('price', { precision: 10, scale: 2 }),
        inStock: mysqlBoolean('in_stock').default(true),
        createdAt: mysqlTimestamp('created_at'),
      });
      return typeof products;
    },
    expect: 'object'
  },

  // Creating a SQLite table schema
  {
    name: 'sqliteTable creates table',
    fn: () => {
      const posts = sqliteTable('posts', {
        id: sqliteInteger('id').primaryKey(),
        title: sqliteText('title').notNull(),
        views: sqliteInteger('views').default(0),
      });
      return typeof posts;
    },
    expect: 'object'
  },

  // SQL template tag
  {
    name: 'sql template creates SQL',
    fn: () => {
      const query = sql`SELECT * FROM users WHERE id = 1`;
      return query !== null && typeof query === 'object';
    },
    expect: true
  },
  {
    name: 'sql.raw creates raw SQL',
    fn: () => {
      const raw = sql.raw('NOW()');
      return raw !== null && typeof raw === 'object';
    },
    expect: true
  },

  // Creating conditions
  {
    name: 'eq creates equality condition',
    fn: () => {
      const users = pgTable('users', { id: serial('id') });
      const condition = eq(users.id, 1);
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },
  {
    name: 'and combines conditions',
    fn: () => {
      const users = pgTable('users', {
        id: serial('id'),
        active: boolean('active'),
      });
      const condition = and(eq(users.id, 1), eq(users.active, true));
      return condition !== null;
    },
    expect: true
  },
  {
    name: 'or combines conditions',
    fn: () => {
      const users = pgTable('users', {
        id: serial('id'),
      });
      const condition = or(eq(users.id, 1), eq(users.id, 2));
      return condition !== null;
    },
    expect: true
  },
  {
    name: 'gt creates greater than condition',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const condition = gt(users.age, 18);
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },
  {
    name: 'lt creates less than condition',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const condition = lt(users.age, 65);
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },
  {
    name: 'between creates range condition',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const condition = between(users.age, 18, 65);
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },
  {
    name: 'like creates pattern condition',
    fn: () => {
      const users = pgTable('users', { name: text('name') });
      const condition = like(users.name, '%john%');
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },
  {
    name: 'inArray creates IN condition',
    fn: () => {
      const users = pgTable('users', { id: serial('id') });
      const condition = inArray(users.id, [1, 2, 3]);
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },
  {
    name: 'isNull creates NULL check',
    fn: () => {
      const users = pgTable('users', { deletedAt: timestamp('deleted_at') });
      const condition = isNull(users.deletedAt);
      return condition !== null && typeof condition === 'object';
    },
    expect: true
  },

  // Order functions
  {
    name: 'asc creates ascending order',
    fn: () => {
      const users = pgTable('users', { name: text('name') });
      const order = asc(users.name);
      return order !== null && typeof order === 'object';
    },
    expect: true
  },
  {
    name: 'desc creates descending order',
    fn: () => {
      const users = pgTable('users', { createdAt: timestamp('created_at') });
      const order = desc(users.createdAt);
      return order !== null && typeof order === 'object';
    },
    expect: true
  },

  // Aggregate functions
  {
    name: 'count creates count aggregation',
    fn: () => {
      const result = count();
      return result !== null && typeof result === 'object';
    },
    expect: true
  },
  {
    name: 'sum creates sum aggregation',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const result = sum(users.age);
      return result !== null && typeof result === 'object';
    },
    expect: true
  },
  {
    name: 'avg creates avg aggregation',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const result = avg(users.age);
      return result !== null && typeof result === 'object';
    },
    expect: true
  },
  {
    name: 'min creates min aggregation',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const result = min(users.age);
      return result !== null && typeof result === 'object';
    },
    expect: true
  },
  {
    name: 'max creates max aggregation',
    fn: () => {
      const users = pgTable('users', { age: integer('age') });
      const result = max(users.age);
      return result !== null && typeof result === 'object';
    },
    expect: true
  },

  // Placeholder for prepared statements
  {
    name: 'placeholder creates parameter placeholder',
    fn: () => {
      const userId = placeholder('userId');
      return userId !== null && typeof userId === 'object';
    },
    expect: true
  },

  // Column modifiers
  {
    name: 'notNull modifier works',
    fn: () => {
      const col = text('name').notNull();
      return col !== null && typeof col === 'object';
    },
    expect: true
  },
  {
    name: 'default modifier works',
    fn: () => {
      const col = boolean('active').default(true);
      return col !== null && typeof col === 'object';
    },
    expect: true
  },
  {
    name: 'unique modifier works',
    fn: () => {
      const col = varchar('email', { length: 255 }).unique();
      return col !== null && typeof col === 'object';
    },
    expect: true
  },
  {
    name: 'primaryKey modifier works',
    fn: () => {
      const col = serial('id').primaryKey();
      return col !== null && typeof col === 'object';
    },
    expect: true
  },

  // Relations and indexes
  {
    name: 'index creates index',
    fn: () => {
      const idx = index('name_idx');
      return idx !== null && typeof idx === 'object';
    },
    expect: true
  },
  {
    name: 'uniqueIndex creates unique index',
    fn: () => {
      const idx = uniqueIndex('email_idx');
      return idx !== null && typeof idx === 'object';
    },
    expect: true
  },

  // Complex schema with relations
  {
    name: 'complex schema with foreign key',
    fn: () => {
      const users = pgTable('users', {
        id: serial('id').primaryKey(),
        name: text('name').notNull(),
      });

      const posts = pgTable('posts', {
        id: serial('id').primaryKey(),
        title: text('title').notNull(),
        authorId: integer('author_id').references(() => users.id),
      });

      return typeof posts === 'object' && 'authorId' in posts;
    },
    expect: true
  },

  // JSON columns
  {
    name: 'json column works',
    fn: () => {
      const settings = pgTable('settings', {
        id: serial('id').primaryKey(),
        config: json('config'),
      });
      return 'config' in settings;
    },
    expect: true
  },
  {
    name: 'jsonb column works',
    fn: () => {
      const events = pgTable('events', {
        id: serial('id').primaryKey(),
        metadata: jsonb('metadata'),
      });
      return 'metadata' in events;
    },
    expect: true
  },

  // UUID column
  {
    name: 'uuid column works',
    fn: () => {
      const items = pgTable('items', {
        id: uuid('id').defaultRandom().primaryKey(),
        name: text('name'),
      });
      return 'id' in items;
    },
    expect: true
  },

  // Timestamp with modes
  {
    name: 'timestamp with mode date',
    fn: () => {
      const col = timestamp('created_at', { mode: 'date' });
      return col !== null && typeof col === 'object';
    },
    expect: true
  },
  {
    name: 'timestamp with withTimezone',
    fn: () => {
      const col = timestamp('created_at', { withTimezone: true });
      return col !== null && typeof col === 'object';
    },
    expect: true
  },
  {
    name: 'timestamp defaultNow',
    fn: () => {
      const col = timestamp('created_at').defaultNow();
      return col !== null && typeof col === 'object';
    },
    expect: true
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Drizzle ORM Compatibility Tests ===\n');

async function runTests() {
  for (const test of tests) {
    try {
      let result = test.fn();
      if (result instanceof Promise) {
        result = await result;
      }
      const resultStr = JSON.stringify(result);
      const expectStr = JSON.stringify(test.expect);

      if (resultStr === expectStr) {
        console.log(`PASS: ${test.name}`);
        passed++;
      } else {
        console.log(`FAIL: ${test.name}`);
        console.log(`  Expected: ${expectStr}`);
        console.log(`  Got: ${resultStr}`);
        failed++;
        failures.push(`${test.name}: expected ${expectStr}, got ${resultStr}`);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      console.log(`ERROR: ${test.name} - ${msg}`);
      failed++;
      failures.push(`${test.name}: ${msg}`);
    }
  }

  console.log('\n=== Summary ===');
  console.log(`Passed: ${passed}/${tests.length}`);
  console.log(`Failed: ${failed}/${tests.length}`);

  if (failures.length > 0) {
    console.log('\nFailures:');
    for (const f of failures) {
      console.log(`  - ${f}`);
    }
  }

  if (failed > 0) {
    process.exit(1);
  }
}

runTests();
