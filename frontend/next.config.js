const path = require('path');
const tiptapPmResolveBase = path.dirname(require.resolve('@tiptap/pm/model'));
const resolveFromTiptapPm = (pkg) =>
  require.resolve(pkg, { paths: [tiptapPmResolveBase] });

// Self-hosted web build: swap the Tauri IPC modules for the HTTP/SSE shim.
// Enabled with NEXT_PUBLIC_TARGET=web (docker/Dockerfile sets it). Desktop unaffected.
const isWebTarget = process.env.NEXT_PUBLIC_TARGET === 'web';
const webShim = (file) => path.resolve(__dirname, 'src/lib/web-shim', file);
const webShimAliases = {
  '@tauri-apps/api/core$': webShim('core.ts'),
  '@tauri-apps/api/tauri$': webShim('core.ts'),
  '@tauri-apps/api/event$': webShim('event.ts'),
  '@tauri-apps/api/app$': webShim('plugins.ts'),
  '@tauri-apps/api/path$': webShim('plugins.ts'),
  '@tauri-apps/plugin-store$': webShim('plugins.ts'),
  '@tauri-apps/plugin-os$': webShim('plugins.ts'),
  '@tauri-apps/plugin-updater$': webShim('plugins.ts'),
  '@tauri-apps/plugin-process$': webShim('plugins.ts'),
};

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: false, // Disabled for BlockNote compatibility
  output: 'export',
  // Web build: emit every route as a directory + index.html so a plain static file
  // server (the Rust binary's ServeDir) resolves /login without extension probing.
  trailingSlash: isWebTarget,
  images: {
    unoptimized: true,
  },
  // Add basePath configuration
  basePath: '',
  assetPrefix: '/',

  // Add webpack configuration for Tauri
  webpack: (config, { isServer }) => {
    if (!isServer) {
      config.resolve.fallback = {
        ...config.resolve.fallback,
        fs: false,
        path: false,
        os: false,
      };

      // Keep ProseMirror single-instanced for BlockNote/Tiptap.
      config.resolve.alias = {
        ...config.resolve.alias,
        '@blocknote/core$': require.resolve('@blocknote/core'),
        '@blocknote/react$': require.resolve('@blocknote/react'),
        '@blocknote/shadcn$': require.resolve('@blocknote/shadcn'),
        'prosemirror-model': resolveFromTiptapPm('prosemirror-model'),
        'prosemirror-state': resolveFromTiptapPm('prosemirror-state'),
        'prosemirror-view': resolveFromTiptapPm('prosemirror-view'),
        'prosemirror-transform': resolveFromTiptapPm('prosemirror-transform'),
        'prosemirror-tables': resolveFromTiptapPm('prosemirror-tables'),
        'prosemirror-schema-list': resolveFromTiptapPm('prosemirror-schema-list'),
        'prosemirror-keymap': resolveFromTiptapPm('prosemirror-keymap'),
        'prosemirror-commands': resolveFromTiptapPm('prosemirror-commands'),
        'prosemirror-history': resolveFromTiptapPm('prosemirror-history'),
        'prosemirror-inputrules': resolveFromTiptapPm('prosemirror-inputrules'),
        'prosemirror-gapcursor': resolveFromTiptapPm('prosemirror-gapcursor'),
        'prosemirror-dropcursor': resolveFromTiptapPm('prosemirror-dropcursor'),
        ...(isWebTarget ? webShimAliases : {}),
      };
    }
    return config;
  },
}

module.exports = nextConfig
