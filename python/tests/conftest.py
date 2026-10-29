import pytest

import wasmo


@pytest.fixture
def aws():
    with wasmo.mock_aws() as aws:
        yield aws
