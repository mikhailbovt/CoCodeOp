import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const officeTools = ['whoami', 'echo', 'probe_create_file'];
const ollamaEndpoint = 'http://host.docker.internal:11434';
const ollamaModel = 'qwen3.5:4b';
const ollamaDigest = 'd8b0f5e9760cd1682034f292d7ef72ec46f432149be0df7574bf2d6e92e38c04';
const [mode, actor, endpoint] = process.argv.slice(2);

function report(data, success) {
  process.stdout.write(`${JSON.stringify(data)}\n`);
  process.exitCode = success ? 0 : 1;
}

function probeNativeWrites(root) {
  const subdir = root?.match(/^\/fixture\/cocodeop-m00-[a-zA-Z0-9]+$/u);
  if (!subdir) throw new Error('Expected the disposable fixture mount path');

  const readOnlyMount = readFileSync('/proc/self/mountinfo', 'utf8')
    .split('\n')
    .map((line) => line.split(' '))
    .some((fields) => fields[4] === '/fixture' && fields[5].split(',').includes('ro'));

  let direct;
  try {
    writeFileSync(join(root, 'native-node.txt'), 'test', { flag: 'wx' });
    direct = 'allowed';
  } catch (error) {
    direct = error.code;
  }
  const child = spawnSync('/bin/sh', [
    '-c', 'set -C; printf test > "$1"', 'sh', join(root, 'native-child.txt'),
  ], { encoding: 'utf8', timeout: 5_000 });
  const subprocessDenied = child.status !== null && child.status !== 0
    && /Read-only file system|Permission denied/u.test(child.stderr);
  report({ readOnlyMount, direct, subprocessDenied },
    readOnlyMount && ['EROFS', 'EACCES'].includes(direct) && subprocessDenied);
}

function makeProfile(backend, url, home) {
  const env = { ...process.env, HOME: home, CODEX_HOME: join(home, '.codex'), NO_COLOR: '1' };
  const cwd = join(home, 'work');
  mkdirSync(cwd);
  const token = env.OFFICE_TOKEN;

  if (backend === 'codex') {
    mkdirSync(env.CODEX_HOME, { mode: 0o700 });
    const mountedAuth = '/run/cocodeop-provider-auth.json';
    if (existsSync(mountedAuth)) {
      const isolatedAuth = join(env.CODEX_HOME, 'auth.json');
      copyFileSync(mountedAuth, isolatedAuth);
      chmodSync(isolatedAuth, 0o600);
    }
    const config = [
      'web_search = "disabled"',
      'cli_auth_credentials_store = "file"',
      '[mcp_servers.office]',
      `url = ${JSON.stringify(url)}`,
      'bearer_token_env_var = "OFFICE_TOKEN"',
      'required = true',
      'startup_timeout_sec = 6',
      'tool_timeout_sec = 8',
      `enabled_tools = ${JSON.stringify(officeTools)}`,
      'default_tools_approval_mode = "approve"',
      '',
    ].join('\n');
    writeFileSync(join(env.CODEX_HOME, 'config.toml'), config, { mode: 0o600 });
  } else if (backend === 'gemini') {
    const configDir = join(home, '.gemini');
    mkdirSync(configDir, { mode: 0o700 });
    writeFileSync(join(configDir, 'settings.json'), JSON.stringify({
      security: { auth: { selectedType: 'gemini-api-key' } },
      mcp: { allowed: ['office'] },
      mcpServers: {
        office: {
          httpUrl: url,
          headers: { Authorization: `Bearer ${token}` },
          includeTools: officeTools,
          timeout: 6_000,
          trust: true,
        },
      },
    }), { mode: 0o600 });
    delete env.OFFICE_TOKEN;
  } else {
    env.OPENCODE_CONFIG = join(home, 'opencode.json');
    env.OPENCODE_DISABLE_AUTOUPDATE = 'true';
    env.OPENCODE_DISABLE_MODELS_FETCH = 'true';
    env.OPENCODE_DISABLE_LSP_DOWNLOAD = 'true';
    writeFileSync(env.OPENCODE_CONFIG, JSON.stringify({
      share: 'disabled',
      snapshot: false,
      enabled_providers: ['ollama'],
      model: `ollama/${ollamaModel}`,
      small_model: `ollama/${ollamaModel}`,
      permission: Object.fromEntries([
        ['*', 'deny'],
        ...officeTools.map((tool) => [`office_${tool}`, 'allow']),
      ]),
      provider: {
        ollama: {
          npm: '@ai-sdk/openai-compatible',
          name: 'Ollama',
          options: { baseURL: `${ollamaEndpoint}/v1` },
          models: { [ollamaModel]: { name: 'Qwen 3.5 4B', tool_call: true, limit: { context: 16_384, output: 2_048 } } },
        },
      },
      mcp: { office: { type: 'remote', url, headers: { Authorization: `Bearer ${token}` }, oauth: false, timeout: 5_000 } },
    }), { mode: 0o600 });
    delete env.OFFICE_TOKEN;
  }

  return { cwd, env };
}

function eventsFrom(output) {
  return output.split('\n').flatMap((line) => {
    try {
      const event = JSON.parse(line);
      return typeof event?.type === 'string' ? [event] : [];
    } catch {
      return [];
    }
  });
}

function codexReceipt(event) {
  const item = event.item;
  if (event.type !== 'item.completed' || item?.type !== 'mcp_tool_call'
      || item.server !== 'office' || item.status !== 'completed') return [];
  const result = item.result;
  const structured = result?.structured_content ?? result?.structuredContent;
  if (structured) return [{ tool: item.tool, value: structured }];

  return (result?.content ?? []).flatMap((content) => {
    try {
      return [{ tool: item.tool, value: JSON.parse(content.text) }];
    } catch {
      return [];
    }
  });
}

function geminiReceipt(event, names) {
  if (event.type === 'tool_use') names.set(event.tool_id, event.tool_name);
  if (event.type !== 'tool_result' || event.status !== 'success') return [];
  const tool = officeTools.find((name) => {
    const actual = names.get(event.tool_id);
    return actual === name || actual === `office__${name}` || actual === `mcp__office__${name}`;
  });
  if (!tool) return [];
  try {
    const output = typeof event.output === 'string' ? JSON.parse(event.output) : event.output;
    return [{ tool, value: output?.structuredContent ?? output }];
  } catch {
    return [];
  }
}

function opencodeReceipt(event) {
  const part = event.part;
  if (event.type !== 'tool_use' || part?.state?.status !== 'completed') return [];
  const tool = officeTools.find((name) => part.tool === `office_${name}`);
  if (!tool || typeof part.state.output !== 'string') return [];
  try {
    return [{ tool, value: JSON.parse(part.state.output) }];
  } catch {
    return [];
  }
}

async function runClient(backend, negative) {
  if (!['lead-a', 'lead-b'].includes(actor) || !/^http:\/\/127\.0\.0\.1:\d+\/mcp$/u.test(endpoint)) {
    throw new Error('Expected an actor and a loopback MCP URL');
  }
  if (!/^[0-9a-f]{64}$/u.test(process.env.OFFICE_TOKEN ?? '')) {
    throw new Error('Missing probe credential');
  }

  const home = mkdtempSync('/tmp/office-probe-home-');
  const url = negative ? 'http://127.0.0.1:1/mcp' : endpoint;
  const profile = makeProfile(backend, url, home);
  // npm installs the platform binary from the lockfile. The wrapper requires a postinstall step.
  const binary = backend === 'opencode' ? `/opt/probe/node_modules/opencode-linux-${process.arch}/bin/opencode` : backend;
  const version = spawnSync(binary, ['--version'], { ...profile, encoding: 'utf8', timeout: 15_000 });
  if (version.status !== 0) {
    report({ backend, actor, phase: 'version', exitCode: version.status }, false);
    return;
  }
  const clientVersion = version.stdout.match(/\b\d+\.\d+\.\d+\b/u)?.[0];
  const text = `CoCodeOp M00 ${actor}`;
  const prompt = `Run this CoCodeOp compatibility check with the office MCP server. Call whoami, then echo with text "${text}", then probe_create_file with the exact same text. After all three calls, report the identity and receipt. Use these three office tools.`;
  const args = backend === 'codex'
    ? [
      '--ask-for-approval', 'never', 'exec', '--strict-config', '--ignore-rules', '--sandbox', 'read-only',
      '--skip-git-repo-check', '--ephemeral', '--color', 'never', '--json', prompt,
    ]
    : backend === 'gemini' ? ['--output-format', 'stream-json', '--prompt', prompt]
      : ['run', '--format', 'json', prompt];

  // Verify the installed Codex sees the intended config from the exact probe cwd.
  let codexServerAllowlist;
  if (backend === 'codex') {
    const catalog = spawnSync('codex', ['mcp', 'list', '--json'], { ...profile, encoding: 'utf8', timeout: 15_000 });
    codexServerAllowlist = catalog.status === 0
      && JSON.parse(catalog.stdout).map((server) => server.name).join(',') === 'office';
    if (!codexServerAllowlist) {
      report({ backend, actor, clientVersion, phase: 'server_allowlist', codexServerAllowlist }, false);
      return;
    }
  }

  let opencodeServerConnected;
  if (backend === 'opencode') {
    const catalog = spawnSync(binary, ['mcp', 'list'], { ...profile, encoding: 'utf8', timeout: 15_000 });
    const catalogText = `${catalog.stdout ?? ''}\n${catalog.stderr ?? ''}`.replace(/\x1b\[[0-9;]*[a-zA-Z]/gu, '');
    opencodeServerConnected = catalog.status === 0 && /\boffice\s+connected\b/iu.test(catalogText)
      && !catalogText.includes('unapproved_fixture');
    if (negative || !opencodeServerConnected) {
      report({ backend, actor, clientVersion, phase: 'required_office', opencodeServerConnected, missingOffice: negative },
        negative && catalog.status === 0 && !opencodeServerConnected);
      return;
    }
    try {
      const response = await fetch(`${ollamaEndpoint}/api/tags`, { signal: AbortSignal.timeout(5_000) });
      const models = (await response.json()).models;
      if (!response.ok || !models.some((model) => model.name === ollamaModel && model.digest === ollamaDigest)) {
        report({ backend, actor, clientVersion, phase: 'ollama_model_not_pinned', opencodeServerConnected }, false);
        return;
      }
    } catch {
      report({ backend, actor, clientVersion, phase: 'ollama_unavailable', opencodeServerConnected }, false);
      return;
    }
  }

  const run = spawnSync(binary, args, { ...profile, encoding: 'utf8', timeout: 100_000, maxBuffer: 2_000_000 });
  const events = eventsFrom(run.stdout ?? '');
  const diagnostics = `${run.stderr ?? ''}\n${run.stdout ?? ''}`;
  const toolIds = new Map();
  const receipts = events.flatMap((event) => {
    if (backend === 'codex') return codexReceipt(event);
    if (backend === 'gemini') return geminiReceipt(event, toolIds);
    return opencodeReceipt(event);
  });
  const verifiedReceipts = receipts.filter(({ tool, value }) => {
    if (value?.actor_id !== actor) return false;
    if (tool === 'echo') return value.text === text;
    if (tool === 'probe_create_file') {
      const hash = createHash('sha256').update(text).digest('hex');
      return value.path === `${actor}.txt` && value.sha256 === hash;
    }
    return tool === 'whoami';
  });
  const verifiedTools = verifiedReceipts.map(({ tool }) => tool);

  const providerAuthError = /401 Unauthorized|API key not valid|API_KEY_INVALID|GEMINI_API_KEY|Missing bearer or basic authentication|incorrect API key/iu.test(diagnostics);
  const officeStartupError = /required MCP server|MCP startup failed|MCP server [`'"]?office[`'"]? failed/iu.test(diagnostics);
  const providerRateLimited = /429 Too Many Requests|insufficient_quota|usage limit|rate limit exceeded/iu.test(diagnostics);
  const providerNetworkError = /reconnecting|stream disconnected|error sending request|connection refused/iu.test(diagnostics);
  const protocol = verifiedReceipts.find(({ tool }) => tool === 'whoami')?.value.mcp_protocol_header;
  const rawUsage = events.findLast(({ type }) => type === 'turn.completed')?.usage;
  const usage = rawUsage && Object.fromEntries(['input_tokens', 'cached_input_tokens', 'output_tokens']
    .filter((key) => Number.isSafeInteger(rawUsage[key])).map((key) => [key, rawUsage[key]]));
  const status = {
    backend,
    actor,
    clientVersion,
    missingOffice: negative,
    codexServerAllowlist,
    opencodeServerConnected,
    ollamaModel: backend === 'opencode' ? ollamaModel : undefined,
    ollamaDigest: backend === 'opencode' ? ollamaDigest : undefined,
    exitCode: run.status,
    processError: run.error?.code,
    events: [...new Set(events.map(({ type }) => type))],
    verifiedTools: [...new Set(verifiedTools)],
    protocolHeader: /^\d{4}-\d{2}-\d{2}$/u.test(protocol ?? '') ? protocol : undefined,
    usage,
    providerAuthError,
    officeStartupError,
    providerRateLimited,
    providerNetworkError,
  };
  const success = negative
    ? run.status !== null && run.status !== 0 && officeStartupError
      && !events.some(({ type }) => type === 'turn.completed' || type === 'result')
    : !run.error && run.status === 0 && officeTools.every((tool) => verifiedTools.includes(tool));
  report(status, success);
}

if (mode === 'native-write-check') {
  probeNativeWrites(actor);
} else if (['codex', 'gemini', 'opencode', 'codex-missing-office', 'opencode-missing-office'].includes(mode)) {
  await runClient(mode.split('-')[0], mode.endsWith('-missing-office'));
} else {
  throw new Error('Expected native-write-check, codex, gemini, or opencode probe');
}
