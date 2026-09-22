Feature: Configuration handling
  The CLI validates the config before any daemon is spawned or contacted,
  and the daemon re-validates at boot. A missing file means defaults; a
  broken file means one precise error and no daemon.

  Scenario: malformed config file fails with a precise error
    Given a fresh state directory
    And a config file containing:
      """
      [profile]
      lock_public_gate = "definitely"
      """
    When I run "yard status"
    Then the command fails
    And the error output mentions "lock_public_gate"
    And no daemon was started

  Scenario: missing config falls back to defaults
    Given a fresh state directory
    When I run "yard status"
    Then the command succeeds
    And the output mentions "no work items yet"
