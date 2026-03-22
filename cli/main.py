# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import typer

from cli.commands import accounts, policy, send, drafts, actions, mcp

app = typer.Typer(
    name="envelope",
    help="Envelope Email CLI — programmable email API",
    no_args_is_help=True,
)

app.add_typer(accounts.app, name="accounts")
app.add_typer(policy.app, name="policy")
app.add_typer(send.app, name="send")
app.add_typer(drafts.app, name="draft")
app.add_typer(actions.app, name="actions")
app.add_typer(mcp.app, name="mcp")


if __name__ == "__main__":
    app()
