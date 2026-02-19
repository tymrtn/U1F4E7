import os
import pytest
from httpx import ASGITransport, AsyncClient

os.chdir(os.path.join(os.path.dirname(__file__), ".."))

from app.main import app


@pytest.fixture
def client():
    transport = ASGITransport(app=app)
    return AsyncClient(transport=transport, base_url="http://test")
