import assert from 'node:assert/strict';
import test from 'node:test';

import { settleToolCatalogResult } from './toolCatalog.logic.ts';
import type { ToolSpec } from '../types/api.ts';

const agentTool: ToolSpec = {
  name: 'shell',
  description: 'Run a shell command',
  parameters: {},
};

test('catalog settling preserves agent tools', () => {
  const result = settleToolCatalogResult({ status: 'fulfilled', value: [agentTool] });

  assert.deepEqual(result.entries.map((entry) => [entry.name, entry.group]), [
    ['shell', 'agent'],
  ]);
  assert.deepEqual(result.warnings, []);
});

test('catalog settling throws when agent tools fail', () => {
  assert.throws(
    () => settleToolCatalogResult({ status: 'rejected', reason: new Error('agent catalog unavailable') }),
    /agent catalog unavailable/,
  );
});
