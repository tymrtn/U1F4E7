import os
import pytest
import pytest_asyncio
from httpx import ASGITransport, AsyncClient

os.chdir(os.path.join(os.path.dirname(__file__), ".."))

from app.db import init_db
from app.main import app


@pytest_asyncio.fixture(autouse=True)
async def setup_db(tmp_path):
    db_path = str(tmp_path / "test.db")
    os.environ["ENVELOPE_DB_PATH"] = db_path
    os.environ.setdefault("ENVELOPE_SECRET_KEY", "test-key-for-ci")
    # Re-set DB_PATH in the module since it reads at import time
    import app.db
    app.db.DB_PATH = db_path
    await init_db()
    yield
    if os.path.exists(db_path):
        os.unlink(db_path)


@pytest.fixture
def client():
    transport = ASGITransport(app=app)
    return AsyncClient(transport=transport, base_url="http://test")
