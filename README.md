# Ferron language server

A language server (LSP) that allows editors to support `ferron.conf` and [Ferron](https://ferron.sh) configuration files to improve configuration editing UX.

**Features:**

- **Diagnostics** (`ferron validate` or `ferron doctor` on save; eager `ferronconf` parse error reporting)
- **Formatting** (`ferron-fmt`)
- **Hover info + completions** (using data from `ferron directives`)

## Installation

```bash
cargo install ferron-language-server
```

or

```bash
npm install -g ferron-language-server
```

## License

[MIT](./LICENSE)
