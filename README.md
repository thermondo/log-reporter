# heroku timeout sentry reporter

## motivation

In our Heroku apps we saw that developers tend to overlook timeouts that happen.
Since we're using [sentry](sentry.io/) for error reporting, we had the idea of
writing a small service that creates sentry issues for each timeout.

We now open the codebase up, with the goal to evolve this service into something
that is useful for more companies.

## design

Heroku supports
[HTTPS log drains](https://devcenter.heroku.com/articles/log-drains). On each of
your Heroku application you can configure a new log drain pointing to this
log-reporter service. This leads to us getting all log messages.

The service then parses the logs for router log lines, timeouts and generates a
sentry report out of it.

So the sentry error grouping works we try to replace some patterns in the path
which we think represent identifiers.

## deployment

Deployment works via `heroku.yml` and the linked `Dockerfile`.

We're currently running the service on _one_ small dyno, and it's handling
around 3k RPM without any issues.

## configuration

Configuration can be set using environment variables, more details in the
`config` module.

### the service itself

- `PORT` (mandatory): normally set by Heroku, the port the webserver runs on
- `SENTRY_DSN` (optional): the sentry DSN where the service should _its own_
  errors to. The sentry client library additional reads some other environment
  variables like `SENTRY_ENVIRONMENT`.
- `SENTRY_DEBUG` (optional): activates sentry debug logging

### mappings for services

You need to add this service as a log-drain to each sender-service:

```bash
$ heroku drains:add https://xxx-log-reporter.herokuapp.com/
...
$ heroku drains
[this will print the _drain token_ you need]
```

Then you can add any mapping as a new environment variable `SENTRY_MAPPING_XXX`
where `XXX` can be replaced with any service name.

The value for this mapping contains of **3** pieces, separated by `|`:

- the logplex token from above
- the destination sentry environment name
- the destination sentry dsn

Example value:

```text
d.xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx|production|https://xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx@sentry.io/9999999
```

## current limitations

This service is running in production at thermondo, but has some pending
improvements, and also contains some internal thermondo specifics around the
replacement patterns in paths.

You can use the service at your own risk, if these specifics are fine with you.
