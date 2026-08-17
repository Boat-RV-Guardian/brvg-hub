// Entry point for the TypeScript hub (Pi / Docker / desktop). Loads config from the environment,
// starts the receiver + drain loop, and shuts down cleanly on a signal.

import { loadConfig, ConfigError } from './config.js';
import { makeSender } from './sender.js';
import { startHub } from './server.js';

function main(): void {
  let config;
  try {
    config = loadConfig(process.env);
  } catch (e) {
    if (e instanceof ConfigError) {
      console.error(`brvg-hub: ${e.message}`);
      process.exit(1);
    }
    throw e;
  }

  const send = makeSender(config);
  const hub = startHub(config, send, (m) => console.error(`brvg-hub: ${m}`));

  const shutdown = () => { void hub.stop().then(() => process.exit(0)); };
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);
}

main();
