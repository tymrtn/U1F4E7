import os
import pytest
import pytest_asyncio
from unittest.mock import MagicMock
from httpx import ASGITransport, AsyncClient

os.chdir(os.path.join(os.path.dirname(__file__), ".."))

import app.db as db_module
from app.db import init_db, close_db
from app.main import app
from app.transport.pool import SmtpConnectionPool


@pytest_asyncio.fixture(autouse=True)
async def setup_db(tmp_path):
    db_path = str(tmp_path / "test.db")
    os.environ["ENVELOPE_DB_PATH"] = db_path
    os.environ.setdefault("ENVELOPE_SECRET_KEY", "test-key-for-ci")
    # Reset singleton and DB_PATH before each test
    db_module._connection = None
    db_module.DB_PATH = db_path
    await init_db()
    # Ensure app has pool and worker stub for tests (lifespan doesn't run with ASGITransport)
    app.state.smtp_pool = SmtpConnectionPool()
    app.state.send_worker = MagicMock()
    app.state.send_worker.notify = MagicMock()
    app.state.inbox_agent = None
    yield
    await app.state.smtp_pool.close_all()
    await close_db()
    if os.path.exists(db_path):
        os.unlink(db_path)


@pytest.fixture
def client():
    transport = ASGITransport(app=app)
    return AsyncClient(transport=transport, base_url="http://test")
