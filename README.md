# ezpez

`ezpez` is a command line tool for running untrusted code inside 
a (micro) VM sandboxed environment. It's meant for *professional* 
software teams and companies to share their sandbox configurations 
in the project's version control. It supports out-of-box:

* Running payloads using VM-level isolation
* Sandbox provisioning and configuration with `ez.toml` file
* Sandbox environment package management with `Dockerfile`
* Network control and filtering with ip / hostname based rules
* Network interception and secrets injection to the http(s) requests
  with a scripting language
* Environment variable injection
* Selective file/directory mounting

## Usage

```bash 
# Initialize ez.toml
ez init 

# Start default command from ez dockerfile
ez 
```

## Architecture

See [design doc](./docs/DESIGN.md) for more details.