# Templates

A template is the user-facing wrapper around a committed snapshot. Instead of
booting a fresh VM and installing software every time, you build a template
once and later create sandboxes from it in milliseconds.

## How Templates Work

1. **Define**: specify an overlaybd-backed base rootfs and ordered steps such as `run`, `env`, and `workdir`.
2. **Build**: AgentENV boots a temporary sandbox and executes those steps inside it.
3. **Finalize**: optional startup commands can be started and checked for readiness before the snapshot is captured.
4. **Publish**: the result is committed as a snapshot in the snapshot repository.
5. **Launch**: creating a sandbox from a template resolves the committed snapshot and resumes from it.

## Create Your Template

There are two ways to create a template: `aenv pull` imports an OCI image directly, and `aenv build` runs Dockerfile instructions inside a temporary build sandbox.

### aenv pull

Pulling an existing OCI image as a template:

```bash
aenv pull ubuntu:24.04
```

| Flag | Description |
|------|-------------|
| `--name <name>` | Template name. Defaults to the repository segment of the image |
| `--start-cmd <cmd>` | Command to run before capturing the snapshot |
| `--ready-cmd <cmd>` | Shell command polled every 2 s until exit 0; gates snapshot capture |
| `--probe <port>` | Readiness check: wait for TCP on `localhost:<port>` |
| `-d`, `--detach` | Submit the build and return without waiting |
| `--timeout <secs>` | Maximum time to wait for the build to complete |

`Env`, `WorkingDir`, and `User` are automatically inherited from the OCI image config. See [Runtime Configuration](#runtime-configuration) for the full field list.

### aenv build

> ⚠️ **Experimental** — Not recommended for production use.

Build a template from a single-stage Dockerfile and a local build context.
The build is orchestrated entirely by the CLI: `aenv` boots an ordinary cold
sandbox from the `FROM` image, executes the instructions in it through envd,
streams `COPY`/`ADD` content directly into the VM (build-context bytes are
never persisted on AgentENV hosts), and captures the result as a named
template:

```bash
aenv build . --name my-template
aenv build ./service --name my-service -f ./service/Dockerfile.prod
```

| Flag | Description |
|------|-------------|
| `[context]` | Build context directory (default `.`) |
| `-f, --file <path>` | Dockerfile path (default `<context>/Dockerfile`) |
| `--name <name>` | Required template name |
| `--image <ref>` | Override the `FROM` image |
| `--cpu`, `--memory`, `--disk-size-mb` | Build sandbox resources (become the template resources) |

Supported Dockerfile instructions:

| Instruction | Behavior |
|-------------|----------|
| `FROM` | Single non-`scratch` base image (overridable with `--image`) |
| `RUN` | Shell-form, exec-form, and heredoc commands executed via `/bin/bash -lc` |
| `ENV` | Set environment variables (multi-key `KEY=value` form supported) |
| `WORKDIR` | Create the directory if needed and set it as the working directory |
| `USER` | Set the execution user for subsequent `RUN` instructions and the template default user |
| `COPY` | Copy files/directories from the local build context (`.dockerignore` honored) |
| `ADD` | Local files/directories only, with `COPY` behavior (no URLs, no auto-extraction) |
| `ENTRYPOINT` | Becomes the template `startCmd` |
| `CMD` | Becomes `startCmd` if no `ENTRYPOINT` is present |
| `EXPOSE` / `VOLUME` / `LABEL` / `STOPSIGNAL` | Warned and ignored |

The build starts the effective `ENTRYPOINT`/`CMD` command in the build VM
before snapshot capture and stores it with an empty ready command. A sandbox
launched from the template therefore resumes the already-started process.

`COPY`/`ADD` content is selected and packaged on the client, streamed through
envd, and extracted inside the guest as root, so copied content is root-owned
(Docker's `--chown`-less default). Destinations without a trailing `/` are
resolved with a guest stat before transfer: an existing directory gets
Docker's copy-into behavior; a missing target becomes a file for one file
source or a directory for one directory source. Ambiguous forms are rejected.
The base image must provide `/bin/bash`, and `tar` when `COPY`/`ADD` is used.

Unsupported forms fail with actionable errors before the build VM is
created: `ARG`, `SHELL`, `FROM scratch`, multi-stage builds, `COPY --from`,
`COPY --chown`/`--chmod`, remote-URL `ADD`, archive auto-extraction,
symlinks and special files in the context, and `HEALTHCHECK`/`ONBUILD`.
Variable expansion (`$VAR`) is not performed outside `RUN`; such values are
used literally with a warning. There is no build cache yet: a retry creates
a fresh build sandbox and re-executes every instruction.

Ordinary background jobs left by a `RUN` step are terminated before the next
step so build-time daemons are not captured accidentally. Processes that
deliberately detach into a new session or process group are outside this v1
contract.

### Runtime Configuration

`aenv pull` reads the OCI image config directly. Native `aenv build` preserves
the base environment, working directory, and numeric user/group observed
inside the build VM, then applies supported Dockerfile instructions. OCI-only
metadata such as base-image ports, volumes, labels, entrypoint, and command is
not inherited unless restated in the Dockerfile. The following fields from the
[OCI image-spec config object](https://github.com/opencontainers/image-spec/blob/main/config.md)
are recognised:

| OCI field | Dockerfile instruction | Runtime effect |
|-----------|------------------------|----------------|
| `Env` | `ENV` | Environment variables injected into every sandbox process |
| `WorkingDir` | `WORKDIR` | Default working directory |
| `User` | `USER` | Default user |
| `Entrypoint` / `Cmd` | `ENTRYPOINT` / `CMD` | Mapped to `startCmd` for `aenv build`; use `--start-cmd` explicitly for `aenv pull` |
| `ExposedPorts` | `EXPOSE` | Stored as metadata by `aenv pull`; ignored by native builds |
| `Volumes` | `VOLUME` | Stored as metadata by `aenv pull`; ignored by native builds |
| `Labels` | `LABEL` | Stored as metadata by `aenv pull`; ignored by native builds |

### Aliases

Each template is identified by a UUID. Pass `--name` at creation time to assign
a human-readable alias:

```bash
aenv pull ubuntu:24.04 --name my-base
aenv build . --name my-service
```

The alias can be used wherever a template ID is accepted:

```bash
aenv start my-base
aenv template delete my-service
```

## Manage Templates

### List templates

```bash
aenv template list        # alias: aenv template ls
```

Displays all templates with their ID, name, build status, CPU, memory, disk size, and last-updated timestamp.

### Delete a template

```bash
aenv template delete <template-id-or-name>   # alias: aenv template rm
```

### Watch a server-side build

`aenv pull -d` submits a server-side template build and returns immediately.
Use `watch` to follow it until it succeeds or fails:

```bash
aenv template watch <template-id-or-name>
```

Native `aenv build` runs synchronously in the CLI and returns after snapshot
capture and build-sandbox cleanup.

### Start a sandbox from a template

```bash
aenv start <template-id-or-name>      # start and attach an interactive shell
aenv start -d <template-id-or-name>   # detach: print sandbox ID and exit
```

## Relationship to Snapshots

Templates are the API and UX layer. Snapshots are the durable runtime layer.

- A template build publishes one committed snapshot.
- A template ID or alias resolves to one committed snapshot.
- A sandbox created from a template resumes from that snapshot.

If you want the storage and runtime model underneath templates, see
[Snapshots](./snapshots.md).
