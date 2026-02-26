# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import typer

app = typer.Typer(help="MCP server commands")


@app.command("serve")
def serve(
    host: str = typer.Option("localhost", "--host"),
    port: int = typer.Option(8765, "--port"),
):
    """Start the Envelope API server with MCP endpoint at /mcp/sse."""
    from cli.config import setup_db
    setup_db()

    typer.echo(f"Starting Envelope API server on {host}:{port}")
    typer.echo(f"MCP SSE endpoint: http://{host}:{port}/mcp/sse")

    import uvicorn
    uvicorn.run(
        "app.main:app",
        host=host,
        port=port,
        reload=False,
    )
