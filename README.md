# Git Release Manager (`grm`)

`grm` installs and updates releases published in Git repositories (currently
GitHub). Each managed release is described by one TOML configuration file.

## Usage

```sh
grm [--configs <DIR>] [-v...]
```

| Option              | Description                                                   |
| ------------------- | ------------------------------------------------------------- |
| `-c, --configs DIR` | Directory containing the configuration files (`*.toml`).      |
| `-v, --verbose`     | Increase verbosity (`-v` info, `-vv` debug, `-vvv` trace).    |

`RUST_LOG` overrides the log level selected with `-v`.

`grm` exits with a non-zero status if the configuration directory cannot be
read or any configuration file is invalid. Valid files are still processed.

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
# runtime_dir = "..."        # lock file; default: $XDG_RUNTIME_DIR/grm
                             # (root: /run/grm)
# data_dir = "..."           # record of installed programs;
                             # default: $XDG_DATA_HOME/grm or
                             # ~/.local/share/grm (root: /var/lib/grm)

[version]
# Named capture groups `major`, `minor` and `patch` are required.
regex = '(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)'   # required
```

All values are validated when loading, and unknown keys are rejected to catch
typos. `runtime_dir` and `data_dir` are created if they do not exist.
See [`configs/`](configs/) for examples.

## License

`grm` is licensed under the GNU General Public License, version 3. See
[`LICENSE`](LICENSE).
