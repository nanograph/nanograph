const { existsSync } = require('fs')
const { join } = require('path')
const { execFileSync } = require('child_process')

function bundledBinaryName() {
  switch (process.platform) {
    case 'darwin':
      switch (process.arch) {
        case 'x64':
          return 'nanograph.darwin-x64.node'
        case 'arm64':
          return 'nanograph.darwin-arm64.node'
        default:
          return null
      }
    case 'linux':
      switch (process.arch) {
        case 'x64':
          return 'nanograph.linux-x64-gnu.node'
        case 'arm64':
          return 'nanograph.linux-arm64-gnu.node'
        default:
          return null
      }
    case 'win32':
      return process.arch === 'x64' ? 'nanograph.win32-x64-msvc.node' : null
    default:
      return null
  }
}

const packageRoot = join(__dirname, '..')
const bundledBinary = bundledBinaryName()
const workspaceNanographManifest = join(packageRoot, '..', 'nanograph', 'Cargo.toml')

if (bundledBinary && existsSync(join(packageRoot, bundledBinary))) {
  console.log(`Using bundled native binary ${bundledBinary}`)
  process.exit(0)
}

if (!existsSync(workspaceNanographManifest)) {
  const targetLabel = bundledBinary ?? `${process.platform}-${process.arch}`
  throw new Error(
    [
      `Missing bundled native binary for ${targetLabel}.`,
      'This installed nanograph-db package is not self-contained: source rebuild requires the monorepo workspace, but ../nanograph is not present here.',
      'Reinstall the package or publish a tarball with the correct bundled .node binary.',
    ].join('\n'),
  )
}

execFileSync(
  process.platform === 'win32' ? 'napi.cmd' : 'napi',
  ['build', '--platform', '--js', 'index.js', '--release'],
  {
    cwd: packageRoot,
    stdio: 'inherit',
  }
)
