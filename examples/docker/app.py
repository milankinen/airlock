from flask import Flask
from valkey import Valkey

app = Flask(__name__)
valkey = Valkey(host='valkey', port=6379)


@app.route('/')
def hello():
    valkey.incr('hits')
    counter = str(valkey.get('hits'), 'utf-8')
    return "This webpage has been viewed " + counter + " time(s)"


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=8000, debug=True)
