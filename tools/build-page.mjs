#!/usr/bin/env node
// Preserve CPA's inline-script delivery, with CPR's native resource/CSP contract.
// Source modules stay separate for maintenance; only generated HTML is published.
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export function buildPage(root) {
  const web = path.join(path.resolve(root), 'web');
  const { version } = JSON.parse(readFileSync(path.join(root, 'plugin.json'), 'utf8'));
  const template = readFileSync(path.join(web, 'ui.html'), 'utf8').replaceAll('{{PLUGIN_VERSION}}', version);
  const html = template.replace(/<script\s+src="([^"]+)"\s*><\/script>/g, (_tag, source) => {
    const filename = path.resolve(web, source);
    if (!filename.startsWith(web + path.sep) || !source.endsWith('.js')) {
      throw new Error('Script source must be a local JavaScript module');
    }
    const script = readFileSync(filename, 'utf8').replace(/<\/script/gi, '<\\/script');
    return '<script>\n' + script + '\n</script>';
  });
  if (/<script\b[^>]*\bsrc\s*=/i.test(html)) throw new Error('External script survived page assembly');
  if (Buffer.byteLength(html) > 1024 * 1024) throw new Error('Entry exceeds CPR one-resource limit');
  return html;
}

export function buildResources(root, destination) {
  const manifest = JSON.parse(readFileSync(path.join(root, 'plugin.json'), 'utf8'));
  const html = buildPage(root);
  for (const [resource, mime] of Object.entries(manifest.resources)) {
    if (mime.includes('javascript') || resource.endsWith('.js')) {
      throw new Error('Published scripts must be inline, not declared as independent fetches');
    }
    const output = path.resolve(destination, resource);
    if (!output.startsWith(path.resolve(destination) + path.sep)) throw new Error('Unsafe output path');
    const body = resource === 'web/ui.html' ? html : readFileSync(path.join(root, resource));
    mkdirSync(path.dirname(output), { recursive: true });
    writeFileSync(output, body);
  }
  return { resources: Object.keys(manifest.resources).length, entry_bytes: Buffer.byteLength(html), external_script_requests: 0 };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = path.resolve(process.argv[2] || '.');
  const manifest = JSON.parse(readFileSync(path.join(root, 'plugin.json'), 'utf8'));
  const destination = path.resolve(process.argv[3] || path.join(root, 'build', 'resources-' + manifest.version));
  console.log(JSON.stringify(buildResources(root, destination)));
}
