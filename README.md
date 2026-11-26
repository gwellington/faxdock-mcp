# FaxDock MCP

Send a local PDF through Telnyx and check delivery from an MCP client. FaxDock runs in one Alpine container and connects to your client over stdio. Receiving faxes is optional.

## Why I built it

I already had the PDF and needed to send one fax. I was looking at about $5 for a single page. I wanted to send it straight to the carrier and be able to check whether it arrived.

## Set up outbound faxing

### 1. Set up Telnyx

In Telnyx Mission Control, create a Programmable Fax application, assign it a fax-capable number, and attach an Outbound Voice Profile. Create a V2 API key and save it in your credential store. Keep the assigned number and the Fax application ID handy. [Telnyx fax quickstart](https://developers.telnyx.com/docs/programmable-fax/quickstart) · [Create an API key](https://developers.telnyx.com/docs/programmable-fax/send-a-fax-api#step-2-api-authentication)

### 2. Start the container

On the machine running your MCP client, install Git and Docker with Compose v2. Use a trusted local Docker context. It receives the Telnyx key when the launcher connects and provides the mounted fax folders. [Docker bind mounts](https://docs.docker.com/engine/storage/bind-mounts/)

```sh
git clone https://github.com/gwellington/faxdock-mcp.git
cd faxdock-mcp
(umask 077; cp .env.example .env; mkdir -p inbox outbox)
```

Fill in these values in `.env`.

| Variable | Value |
| --- | --- |
| `TELNYX_FAX_FROM` | The assigned number in E.164 format, for example `+12025550100`. |
| `TELNYX_FAX_CONNECTION_ID` | The numeric ID of the Programmable Fax application. |

On Linux, set `FAX_UID` and `FAX_GID` in `.env` to the non-root owner of `inbox/` and `outbox/`. If you created the folders as your own user, get the values with `id -u` and `id -g`.

```sh
docker compose up -d --build --wait
docker compose ps
```

The `faxdock` service should be running with no published ports. If you run multiple copies on the same Docker host, set a different `COMPOSE_PROJECT_NAME` in each checkout's `.env`.

### 3. Connect the credential store

**1Password.** Install and sign in to the `op` CLI on the host. Copy the secret reference for the field containing the Telnyx key. Replace the placeholder below with that reference, then run these commands from the checkout. The last command checks that 1Password can resolve it without displaying the key. [1Password CLI secret references](https://www.1password.dev/cli/secret-references)

```sh
(umask 077; printf '%s\n' 'op://VAULT/ITEM/FIELD' > .telnyx-key-ref)
chmod 600 .telnyx-key-ref
op read "$(cat .telnyx-key-ref)" > /dev/null
```

**Other credential stores.** Set `FAX_KEY_COMMAND` in the MCP launch environment to the absolute path of an executable that prints the key to stdout. FaxDock runs it with no arguments. Keep the key in the credential store. The launcher passes it to the MCP process over stdin.

### 4. Connect your MCP client

In Codex, add this block to `~/.codex/config.toml` and replace the example path with the absolute path to your checkout. Start a new session after saving. [Codex MCP setup](https://developers.openai.com/codex/mcp)

```toml
[mcp_servers.telnyx_fax]
command = "/bin/sh"
args = ["/absolute/path/to/faxdock-mcp/start-fax-mcp.sh"]
startup_timeout_sec = 75
default_tools_approval_mode = "writes"
omit_tools_from = ["code_mode"]
```

If you're using a credential helper, add the following table under the same server entry. You can also set `FAX_DOCKER_BIN` or `FAX_OP_BIN` here if Codex can't find those commands on its `PATH`.

```toml
[mcp_servers.telnyx_fax.env]
FAX_KEY_COMMAND = "/absolute/path/to/read-telnyx-key"
```

For other clients, launch `/bin/sh /absolute/path/to/faxdock-mcp/start-fax-mcp.sh` over stdio. **Require human approval for `fax_send` and `fax_cancel` in the client.** FaxDock checks that the send matches the preview. The client enforces approval. The Codex example prompts on writes and keeps these tools out of Code Mode. [Codex configuration reference](https://developers.openai.com/codex/config-reference)

Run `fax_account_check`. For outbound faxing, look for `authenticated`, `application_active`, and `outbound_profile_configured` all set to `true`. This check makes a read-only call to Telnyx.

## Send a fax

1. Put the PDF in `outbox/`. Use a simple filename such as `request.pdf` and keep it at or below 20 MB.
2. Verify the fax number with the recipient. Ask your client to run `fax_preview` with the filename and number in E.164 format, then show you the result. Preview checks the file header, size, and number format.
3. Approve `fax_send` after reviewing the filename and number. The ten-minute preview binds the exact file bytes and recipient and is valid for one send attempt in the same session.
4. Use `fax_status` to check for `delivered`. If the submission result is uncertain, check `fax_history` before trying again. After requesting cancellation, check `fax_status` for the outcome.

FaxDock uploads the file directly to Telnyx and leaves the local copy in `outbox/`.

## Enable incoming faxes

Receiving uses the same container and requires Tailscale Funnel and Docker Engine 28 or newer. [Docker port publishing](https://docs.docker.com/engine/network/port-publishing/) · [Tailscale Funnel](https://tailscale.com/docs/features/tailscale-funnel)

1. Uncomment `COMPOSE_FILE=compose.yaml:compose.inbound.yaml` in `.env` and set `TELNYX_WEBHOOK_PUBLIC_KEY` to the Telnyx webhook signing public key.
2. Run `docker compose up -d --wait` to start the receiver.
3. Run the Funnel command below. In the Fax application's inbound settings, set the webhook URL to the resulting public HTTPS URL ending in `/telnyx/webhook`.

```sh
tailscale funnel --bg --https=443 --set-path=/telnyx/webhook http://127.0.0.1:8787/telnyx/webhook
```

If you set `TELNYX_WEBHOOK_HOST_PORT` in `.env`, use that port in the Funnel target. Keep the Docker port bound to loopback so the receiver can trust Funnel's forwarding headers. The receiver also checks the Telnyx source IP, signature, and timestamp.

Incoming PDFs land in `inbox/<fax-id>.pdf`. Use `fax_inbox` to list local files or `fax_history` with `direction: "inbound"` to see records at Telnyx. Restrict access to the host folders and Docker daemon. Files in both folders stay there until you remove them. [Telnyx inbound fax documentation](https://developers.telnyx.com/docs/programmable-fax/receive-a-fax-api)

## Test and license

Run `docker build --target test .` to test sends, previews, webhook signatures, and inbox storage with synthetic data and a local mock API.

FaxDock is an independent project. Its source is available under the [MIT license](LICENSE).
