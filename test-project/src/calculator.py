"""A simple calculator module with intentional SonarQube issues."""

import os
import sys  # unused import (CODE_SMELL)


def divide(a, b):
    """Divide a by b."""
    # BUG: no check for division by zero
    return a / b


def calculate_discount(price, discount):
    """Calculate discounted price."""
    # CODE_SMELL: cognitive complexity, nested conditions
    if price > 0:
        if discount > 0:
            if discount < 100:
                result = price * (1 - discount / 100)
                if result > 0:
                    if result < price:
                        return result
                    else:
                        return price
                else:
                    return 0
            else:
                return 0
        else:
            return price
    else:
        return 0


def read_config(path):
    """Read configuration from a file."""
    # VULNERABILITY: path traversal - no sanitization
    f = open(path, 'r')
    content = f.read()
    # BUG: file handle not closed (resource leak)
    return content


def process_data(data):
    """Process a list of data."""
    # CODE_SMELL: empty except block
    try:
        result = []
        for item in data:
            result.append(int(item))
        return result
    except:
        pass


def get_user_info(user_id):
    """Get user info by ID."""
    # VULNERABILITY: SQL injection
    import sqlite3
    conn = sqlite3.connect('users.db')
    cursor = conn.cursor()
    query = "SELECT * FROM users WHERE id = " + str(user_id)
    cursor.execute(query)
    return cursor.fetchone()


def format_name(first, last):
    """Format a full name."""
    # CODE_SMELL: duplicate string concatenation
    if first and last:
        name = first + " " + last
        greeting = "Hello, " + first + " " + last + "!"
        return name, greeting
    elif first:
        name = first
        greeting = "Hello, " + first + "!"
        return name, greeting
    else:
        return "", "Hello!"


PASSWORD = "admin123"  # VULNERABILITY: hardcoded credential


def authenticate(user, pwd):
    """Check credentials."""
    # VULNERABILITY: hardcoded password comparison
    if user == "admin" and pwd == PASSWORD:
        return True
    return False
