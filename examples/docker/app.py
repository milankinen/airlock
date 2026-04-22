# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "flask==3.1.2",
#     "valkey==6.1.1",
# ]
# ///

from flask import Flask
from valkey import Valkey

app = Flask(__name__)
valkey_client = Valkey(host="valkey", port=6379)


@app.route("/")
def hello():
    valkey_client.incr("hits")
    counter = str(valkey_client.get("hits"), "utf-8")
    return "This webpage has been viewed " + counter + " time(s)"


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=8000, debug=True)
