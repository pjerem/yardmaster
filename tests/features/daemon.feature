Feature: Daemon lifecycle
  The daemon is not a system service: the first CLI invocation spawns it on
  demand, it survives its launcher, and exactly one instance owns a state
  directory at a time.

  Scenario: status auto-starts the daemon
    Given a fresh state directory
    When I run "yard status"
    Then the command succeeds
    And the daemon is running
    And the lock file exists
    And the socket file exists

  Scenario: double start is a no-op
    Given a fresh state directory
    And a daemon started with "yard daemon run"
    When I run "yard daemon run"
    Then the command succeeds
    And the daemon pid is unchanged
    And the daemon is running

  Scenario: the daemon survives its launcher
    Given a fresh state directory
    When I run "yard status"
    Then the command succeeds
    And the daemon is running
    When the launcher process has exited
    And I run "yard status"
    Then the command succeeds
    And the daemon pid is unchanged

  Scenario: stop terminates cleanly
    Given a fresh state directory
    When I run "yard status"
    Then the command succeeds
    When I run "yard daemon stop"
    Then the command succeeds
    And the socket file is gone
    And the lock file is gone
    And the event log records "daemon_stopped"

  Scenario: stopping without a daemon is not an error
    Given a fresh state directory
    When I run "yard daemon stop"
    Then the command succeeds
    And the output mentions "no daemon running"
