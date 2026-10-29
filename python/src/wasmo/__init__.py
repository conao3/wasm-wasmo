"""Stateful in-process AWS mock for boto3, powered by a single WebAssembly module."""

from wasmo._mock import ENDPOINT, REGION, Mock, Response, mock_aws

__all__ = ["ENDPOINT", "REGION", "Mock", "Response", "mock_aws"]
