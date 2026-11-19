# savoia-studio

## 0.2.0

### Minor Changes

- [#17](https://github.com/joaoGabriel55/savoia-studio/pull/17) [`15b2b5d`](https://github.com/joaoGabriel55/savoia-studio/commit/15b2b5d0532b453b5905100d6ab27f9cb74c8039) Thanks [@tonyaraujop](https://github.com/tonyaraujop)! - Double-click a table or view in the explorer to open its data. A new console (⌘T, Ctrl+T) takes the keyboard at once, and the app's shortcuts keep working after a tab with focus closes. **Ctrl+Shift+F** formats the script, or the statements in the selection: one clause per line, keywords in capitals, names left as typed. Fixed: Run and completion crashed on a block comment with accented text, such as `/* relatório */`.

### Patch Changes

- [#17](https://github.com/joaoGabriel55/savoia-studio/pull/17) [`1c3c945`](https://github.com/joaoGabriel55/savoia-studio/commit/1c3c9455f8db9a19bda5a9d72d6d4b8a2fe3b72e) Thanks [@tonyaraujop](https://github.com/tonyaraujop)! - Fixed: the app icon was missing on Linux and Windows. The window, taskbar and dock now show the Savoia shield, as do the Windows installer and its shortcuts.

## 0.1.1

### Patch Changes

- [#12](https://github.com/joaoGabriel55/savoia-studio/pull/12) [`454a8c4`](https://github.com/joaoGabriel55/savoia-studio/commit/454a8c41401ca6b64a74625eb78db62a76578063) Thanks [@joaoGabriel55](https://github.com/joaoGabriel55)! - Linux packages are smaller and install on minimal systems: the `.deb` and `.rpm` drop from 21.6 MB and 16.3 MB to 14.7 MB each, and both now declare the libraries Savoia needs (xcb, xkbcommon, fontconfig, Vulkan, Wayland).

## 0.1.0

The first release of Savoia Studio, a native database client for PostgreSQL and MySQL.

- **Explore:** a lazy schema tree with counts, ER diagrams, table structure and DDL.
- **Query:** console tabs, run the statement at the caret or the whole script, schema-aware completion with join suggestions, streamed results with cancel, multiple result sets, sort/filter/copy/export, and query history.
- **Data view:** browse, filter and sort tables without SQL; add columns through foreign keys; summaries; edits held as pending changes and committed as previewed SQL in one transaction; read-only mode.
- **Dump and import:** uses your installed `pg_dump`/`psql`/`mysqldump`/`mysql` or a built-in engine; SQL scripts and CSV import; works through SSH tunnels.
- **Connections:** SSL modes, SSH tunnels with known_hosts checks, saved passwords in a user-only file.
- **Polish:** command palette (⌘⇧P), keyboard shortcuts, a Settings screen, light and dark themes that can follow the OS, signed auto-updates from GitHub Releases, and opt-in crash reports you review before sending.
