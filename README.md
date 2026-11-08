# Git Release Manager (`grm`)

`grm` installs and updates releases published in Git repositories (currently
GitHub). Each managed release is described by one TOML configuration file.

## Usage

```sh
grm [--configs <DIR>] [-v...] <COMMAND>
```

| Command             | Description                                                    |
| ------------------- | -------------------------------------------------------------- |
| `list`              | List all packages with their installed versions.               |
| `install <PACKAGE>` | Install the newest release of a package not installed yet.     |
| `update`            | Update all installed packages to their newest releases.        |
| `remove <PACKAGE>`  | Remove an installed package (its binary and recorded version). |

A package is named after its installed binary (`name` in its configuration),
so names must be unique. Its installed version is recorded in `data_dir`, and
binaries are replaced atomically, so running programs are not affected. `grm`
only replaces or removes binaries it installed itself: `install` fails if a
file already exists at the installation path.

| Option              | Description                                                   |
| ------------------- | ------------------------------------------------------------- |
| `-c, --configs DIR` | Directory containing the configuration files (`*.toml`).      |
| `-v, --verbose`     | Increase verbosity (`-v` info, `-vv` debug, `-vvv` trace).    |

`-v` only affects messages of `grm` itself. Settings for modules in `RUST_LOG`
take precedence (e.g. `RUST_LOG=grm=warn`) and can enable messages of
dependencies (e.g. `RUST_LOG=ureq=debug`).

If `GITHUB_TOKEN` is set, it is sent to the GitHub API (and only there). This
raises GitHub's limit of 60 unauthenticated requests per hour and allows
private repositories.

`grm` exits with a non-zero status if the configuration directory cannot be
read, any configuration file is invalid, or any package fails. Valid files and
the other packages are still processed.

## Configuration

`grm` loads every `*.toml` file in the configuration directory, in alphabetical
order. The default directory is `/etc/grm` when running as root, otherwise
`$XDG_CONFIG_HOME/grm` (falling back to `~/.config/grm`).

```toml
[repository]
url = "https://github.com/go-acme/lego/releases"   # required
# type = "github"            # derived from the URL if omitted

[installation]
name = "lego"                # required: file name of the installed binary
# dir = "..."                # default: ~/.local/bin (root: /usr/local/bin)
# mode = 0o755               # permission bits of the installed binary
# runtime_dir = "..."        # lock file `.<name>.lock`;
                             # default: $XDG_RUNTIME_DIR/grm
                             # (root: /run/grm)
# data_dir = "..."           # record of installed programs;
                             # default: $XDG_DATA_HOME/grm or
                             # ~/.local/share/grm (root: /var/lib/grm)

[asset]
# Named capture groups `major`, `minor` and `patch` are required.
version = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)' # required
# Release asset to install; `${name}` is replaced by the group `name`.
target = 'lego_v${major}.${minor}.${patch}_linux_amd64.tar.gz' # required
# File to install from a .tar.gz, .tgz or .zip asset (path inside the archive;
# may use `${name}` like `target`). If omitted, the asset itself is installed.
unpack = 'lego'
```

All values are validated when loading, and unknown keys are rejected to catch
typos. `runtime_dir` and `data_dir` are created if they do not exist.
See [`configs/`](configs/) for examples.

## Security

Downloads use HTTPS only and must match the size reported by the hosting
service. When unpacking, only the configured file is extracted, links and
directories are rejected, and the extracted file is limited to 1 GiB.
Releases are not verified against checksums or signatures, so `grm` trusts
the repository's maintainers and hosting service.

## License

`grm` is licensed under the GNU General Public License, version 3. See
[`LICENSE`](LICENSE).
