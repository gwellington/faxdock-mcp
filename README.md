# FaxDock MCP

Self-hosted Telnyx faxing for MCP clients. Stage a PDF, preview the exact file and recipient, send it directly to Telnyx, and track delivery. One Alpine container runs the Rust binary. The MCP client connects locally over standard input and output. You can also enable an incoming fax inbox.

## Why I built it

I needed to fax a sensitive form I already had as a PDF. Paying for a one-off upload service felt excessive, and I wanted to know where the document went and whether it arrived. FaxDock keeps the PDF in a local outbox until submission and uploads it directly to Telnyx without hosting a download URL.

## Set up outbound faxing

You need Docker with Compose and a Telnyx Fax API application with a fax-capable number and outbound voice profile. [Telnyx Fax API setup](https://developers.telnyx.com/docs/programmable-fax/send-a-fax-api)

```sh
git clone https://github.com/gwellington/faxdock-mcp.git
cd faxdock-mcp
(umask 077; cp .env.example .env; mkdir -p inbox outbox)
```

Set `TELNYX_FAX_FROM` to the assigned number in E.164 format and `TELNYX_FAX_CONNECTION_ID` to the application ID in `.env`. On Linux, use `id -u` and `id -g` to look up the non-root owner of your bind-mounted folders, then set those values as `FAX_UID` and `FAX_GID`.

```sh
docker compose up -d --build --wait
```

### Configure your credential provider

With 1Password, save your Telnyx V2 API key in a password field and put only its secret reference in `.telnyx-key-ref` with mode `0600`. The host launcher uses `op read` with your configured 1Password authentication. [1Password CLI secret references](https://developer.1password.com/docs/cli/secret-references/)

With another credential store, set `FAX_KEY_COMMAND` in the MCP launch environment to the absolute path of an executable that prints the key to stdout and takes no arguments. The launcher passes the key over stdin to the MCP process. Keep it out of the Compose environment, image, and command-line arguments.

### Connect an MCP client

Use `start-fax-mcp.sh` as a local stdio launcher. For Codex, use the following configuration with the absolute path to your checkout.

```toml
[mcp_servers.telnyx_fax]
command = "/bin/sh"
args = ["/absolute/path/to/faxdock-mcp/start-fax-mcp.sh"]
startup_timeout_sec = 75
default_tools_approval_mode = "writes"
omit_tools_from = ["code_mode"]
```

The launcher uses `docker` and `op` from `PATH`. If your client has a restricted `PATH`, set `FAX_DOCKER_BIN` and `FAX_OP_BIN` to their absolute paths in the launch environment. The launcher uses the active Docker context.

**Configure your MCP client to require human approval before `fax_send` and `fax_cancel`.** FaxDock checks the preview and file contents. The client is responsible for asking the human and enforcing their decision.

### Send and track a fax

1. Run `fax_account_check` to verify Telnyx authentication and outbound configuration.
2. Place a PDF, such as `request.pdf`, in `outbox/`. Run `fax_preview` with the exact filename and verified receiving number.
3. Review the proposed send and approve `fax_send`. The preview expires after ten minutes and binds the filename, bytes, and destination within that MCP session.
4. Use `fax_status` to check for `delivered`. If the submission result is uncertain, check `fax_history` before attempting another send. After a `fax_cancel` request, check `fax_status` for the outcome.

## Enable incoming faxes

The standard setup publishes no ports. To receive faxes, use the supplied Tailscale Funnel configuration with Docker Engine 28 or newer. [Docker port publishing](https://docs.docker.com/engine/network/port-publishing/) · [Tailscale Funnel](https://tailscale.com/docs/features/tailscale-funnel)

1. Uncomment `COMPOSE_FILE=compose.yaml:compose.inbound.yaml` in `.env` and set `TELNYX_WEBHOOK_PUBLIC_KEY` to the Telnyx webhook signing public key.
2. Run `docker compose up -d --wait` to start the receiver in the same container.
3. Publish the webhook route through Funnel. Set the resulting HTTPS URL ending in `/telnyx/webhook` on the Telnyx Fax API application.

```sh
tailscale funnel --bg --https=443 --set-path=/telnyx/webhook http://127.0.0.1:8787/telnyx/webhook
```

If you set `TELNYX_WEBHOOK_HOST_PORT` in `.env`, use that port in the Funnel target. Keep the published Docker port on loopback. The receiver trusts Funnel to set the forwarding headers and verifies the Telnyx source IP, signature, and timestamp before using the event.

FaxDock downloads a validated inbound PDF to `inbox/<fax-id>.pdf`. Use `fax_inbox` for local file metadata or `fax_history` with `direction: "inbound"` for Telnyx records. Control access to the Docker daemon and host folders, and manage retention and backups for `inbox/` and `outbox/`. Telnyx processes the PDFs for transmission and reception. [Telnyx inbound fax documentation](https://developers.telnyx.com/docs/programmable-fax/receive-a-fax-api)

## Test and license

Run `docker build --target test .` to exercise the preview and direct upload, webhook signature, and inbox safeguards with synthetic data and a local mock API.

FaxDock is an independent project. Its source is available under the [MIT license](LICENSE).
