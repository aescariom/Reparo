"""Tests for calculator module."""
import pytest
from src.calculator import format_name


def test_format_name_both():
    name, greeting = format_name("John", "Doe")
    assert name == "John Doe"
    assert greeting == "Hello, John Doe!"


def test_format_name_first_only():
    name, greeting = format_name("John", "")
    assert name == "John"
    assert greeting == "Hello, John!"


def test_format_name_empty():
    name, greeting = format_name("", "")
    assert name == ""
    assert greeting == "Hello!"
