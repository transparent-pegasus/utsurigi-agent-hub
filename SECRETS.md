# Secret delivery options

Use this during [INIT](INIT.md#secrets-and-transfer-before-externalization).
The order is **secrets SaaS connector → Bot service's Vault → other secure host
facilities → attached `.env`**. Reuse an approved working choice for the same
owner, Hub, and purpose. A product name or connected account does not prove that
it can inject secrets into this Bot's execution process.
Tier 2 means a Vault supplied and managed by the Bot service; a separately
administered self-hosted Vault is a tier-3 facility.

The options below were checked against official documentation on 2026-11-03.
Availability, permissions, and scheduling behavior still need the two INIT tests
in the actual environment. These are host integration choices; the Hub CLI does
not implement provider login, resolve secret references, or transfer files through
SaaS APIs itself.

## Ranked candidates

| Priority | Method | How it reaches the CLI | Conditions to verify |
|---|---|---|---|
| 1 | Native secrets SaaS connector | The connector passes a secret handle to a trusted executor which resolves it directly into process variables. | Confirm it supports this flow, scoped access, and scheduled execution. A connector that only returns the value in tool/chat text is unsuitable. |
| 1 | 1Password CLI / Environments | `op run` resolves references or an Environment into the child's environment. | Prefer a scoped service account for automation; its bootstrap authentication must itself arrive securely. Desktop approval can suit assisted operation. See [secret loading](https://developer.1password.com/docs/cli/secrets-scripts/) and [service accounts](https://developer.1password.com/docs/service-accounts/). |
| 1 | Bitwarden Secrets Manager | `bws run` injects the selected project's secrets as environment variables. | Use a machine account with access only to the needed project; deliver its access token outside model-visible text. See [CLI documentation](https://bitwarden.com/help/secrets-manager-cli/). |
| 1 | Doppler / Infisical SaaS | Their CLI or supported runtime integration fetches secrets and starts the child with its environment populated. | Scope the project/environment and machine identity; check the installed CLI's commands and unattended authentication. See [Doppler CLI](https://docs.doppler.com/docs/cli) and [Infisical process injection](https://infisical.com/docs/documentation/guides/local-development). |
| 2 | Bot service's built-in Vault / Secrets | The service injects selected secrets into its worker or execution sandbox. | Prove the Bot tool, scheduled job, and recovery worker receive them. Do not assume connector authentication or editor secrets reach every subprocess. [Replit Secrets](https://docs.replit.com/core-concepts/project-editor/app-setup/secrets) is one documented environment-variable facility. |
| 2 | Hosted app secrets attached to the Bot runtime | The host binds secrets to the server-side application process. | For example, [Hugging Face Spaces](https://huggingface.co/docs/hub/spaces-overview#managing-secrets-and-environment-variables) supports secrets, with different behavior by SDK. Client-side/static applications are not a secret execution boundary. Verify a recoverable copy outside the runtime. |
| 3 | Cloud secret manager with workload identity | A trusted host adapter reads the secret through the provider SDK and launches the CLI with only required values. | Prefer existing workload identity over long-lived bootstrap keys. Examples: [AWS Secrets Manager retrieval](https://docs.aws.amazon.com/secretsmanager/latest/userguide/retrieving-secrets.html), [Google authentication](https://cloud.google.com/secret-manager/docs/authentication), [Azure managed identity](https://learn.microsoft.com/en-us/azure/key-vault/general/authentication). This requires a host adapter, not printing a CLI retrieval result. |
| 3 | Existing self-hosted Vault | Vault Agent process supervisor injects `env_template` values into its child. | Verify auto-auth, authorization, server recovery, and restart policy. An automatic restart must not blindly repeat an uncertain backup write. See [Vault Agent](https://developer.hashicorp.com/vault/docs/agent-and-proxy/agent/process-supervisor). |
| 3 | Existing OS/service/container credential facility | A trusted launcher reads a private credential handle/file and maps it to the configured variable names before starting the CLI. | Examples: [systemd credentials](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#Credentials), [Docker Swarm secrets](https://docs.docker.com/engine/swarm/secrets/), or [Kubernetes Secrets](https://kubernetes.io/docs/concepts/configuration/secret/). Check access and encryption: Kubernetes Secret objects are not encrypted at rest by default. Mounted secrets do not automatically become the Hub's password variables. |
| 3 | Existing OS keychain, interactive secure entry, or encrypted file transfer | A host-supported launcher obtains values privately, or decrypts an authorized attachment directly into private storage/process memory. | Investigate only facilities actually installed; prove unlocking/decryption, bootstrap-key recovery, and scheduler access. Interactive-only delivery remains assisted. Do not install a new secret-management stack merely to avoid the fallback. |
| Last | Private `.env` attachment | The owner transfers a file; the host places it beside the config with mode 0600; the CLI loads it on startup. | Use only when all higher choices are unavailable or cannot be selected, and a non-secret dummy attachment proves that contents do not enter model context or conversation records. Unknown or auto-ingesting routes are unavailable. Follow [placement, cleanup, and retention](INIT.md#attached-env-fallback). This retains plaintext locally and may leave provider attachment copies until deleted. |

SaaS storage plus a host Vault can be combined: the SaaS remains the recovery
authority, while the Vault supplies a scoped bootstrap token or process injection.
That does not permit plaintext bootstrap copies. Check account identity, Hub/Bot
scope, revocation, and which users can execute code in that context. Environment
injection protects values from normal conversation output, not from code running
with the same privileges. Never use `printenv`, tracing, or secret-returning tool
calls to demonstrate that injection worked.

## Match the method to this CLI

Configure `repositories.shared.password_env`, `repositories.bot.password_env`, and
any authorized peer's registry `password_env` (for `pull` and `live request`) to
the variable names the launcher will supply. Names are configurable;
`HUB_SHARED_PASSWORD` and `HUB_BOT_PASSWORD` are examples, not built-ins.
`password_ref` and `storage.credentials_ref` record recoverable references only.
The CLI does not fetch them.
Forwarded connection/system names and the three AWS credential names in
[.env.example](.env.example) are reserved for their documented purpose and cannot
be active password names, regardless of the selected storage provider. Use
dedicated names in both Bot YAML and the registry.

With 1Password, for example, a host can maintain a reference-only
`secret-refs.env` and start the CLI as follows (paths are placeholders):

```sh
op run --env-file /PRIVATE/secret-refs.env -- utsurigi-agent-hub --config /PRIVATE/config.yaml status
```

This shows the launch shape, not a successful injection test: `status` does not
open repositories. Use the authenticated operation and transfer test in INIT.
Keep reference files distinct from the fallback `.env`; unresolved `op://` strings
are ordinary literal values to the Hub's dotenv loader. Do not put secret values
in arguments, YAML, or the above command. Auth for `op` is the host's responsibility.

R2 storage needs `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally
`AWS_SESSION_TOKEN`; `google-drive` and `local` need none, as the Bot's connector
handles Drive access. Provider bootstrap tokens are not restic credentials and
are not forwarded to restic or acquisition curl. Each restic command receives only
its selected repository password as `RESTIC_PASSWORD`; peer password names are
not copied wholesale to children.

The CLI forwards these connection settings to restic and acquisition curl:
`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, their lowercase forms,
`SSL_CERT_FILE`, `SSL_CERT_DIR`, and `CURL_CA_BUNDLE`. Keep proxy credentials as
private as other secrets. Use absolute certificate paths. Forwarding preserves
each tool's semantics. Before dotenv values are applied, the CLI checks both
spellings of each proxy pair (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`
and their lowercase forms). If either exists in the original process environment,
even empty, neither spelling from `.env` is added. Otherwise both file spellings
remain available with their native tool precedence. Curl ignores uppercase
`HTTP_PROXY`, and its `ALL_PROXY` and `CURL_CA_BUNDLE` are not substitutes for
restic's HTTP(S) proxy and CA settings.
See [curl proxies](https://everything.curl.dev/usingcurl/proxies/env.html),
[curl CA variables](https://curl.se/docs/sslcerts.html),
[Go HTTP proxy behavior](https://pkg.go.dev/net/http#ProxyFromEnvironment), and
[Go certificate roots](https://pkg.go.dev/crypto/x509#SystemCertPool).
The complete input list and dotenv syntax are in [.env.example](.env.example).
Native `RESTIC_CACERT` and `RESTIC_TLS_CLIENT_CERT` filenames are forwarded only to
storage restic commands; other ambient `RESTIC_*` controls remain filtered. Their
meaning is documented by `restic 0.19.1 --help`. Client-key files remain private.

When invoking restic directly for `check` or snapshot inspection, the Hub CLI's
dotenv loader is not involved. The host must use the same verified secret launcher
or a trusted executor that reads the private fallback file as literal data and
maps the selected YAML `password_env` value to `RESTIC_PASSWORD` in memory.
For R2 it also supplies `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and any
`AWS_SESSION_TOKEN`, plus the listed proxy/CA variables and native restic TLS
filenames (`RESTIC_CACERT`, `RESTIC_TLS_CLIENT_CERT`) needed for that route.
Preserve process-environment priority and the private file checks in this path too.
Never shell-source `.env`, interpolate its values into a command string, or show
raw restic diagnostics containing authenticated URLs.

## Record a decision that can be recovered

Keep a non-secret record of the selected method, account/project/Vault, variable
names and references, repository URLs, scope, scheduler context, host-loss recovery
steps, and the results of both INIT tests. Distinguish authentication success from
successful upload/download and restored-byte equality. If either test fails,
the method is still pending and original data stays local.

For an empty repository destination, create a password in the chosen secret-sharing
service only after verifying emptiness. For any existing data or uncertain state,
request the existing password through that service or the approved fallback.
Recreating a password cannot recover existing ciphertext. Keep recovery credentials
separate from the encrypted backup and retain those needed by older snapshots.
