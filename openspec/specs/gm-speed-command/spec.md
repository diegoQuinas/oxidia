# gm-speed-command Specification

## Purpose

GM command to modify a player's movement speed at runtime. No player param targets self. Speed changes push a `0xA0` stats packet to the target and use remove+re-introduce for spectator broadcast.

## Requirements

### Requirement: Player Speed Attribute

Each player MUST have a `speed` attribute defaulting to `220`. The `creature_speed()` function MUST read this attribute for players (monsters unchanged).

#### Scenario: Default speed on creation
- GIVEN a new player connects
- WHEN the player is fully initialized
- THEN their `speed` MUST be `220`

#### Scenario: Creature speed reads player attribute
- GIVEN a player with speed set to `500`
- WHEN `creature_speed()` is called with that player's id
- THEN it MUST return `500`

### Requirement: Speed Command — Self-Target

The form `/speed <value>` MUST set the issuing GM's own speed.

#### Scenario: Self-target sets speed
- GIVEN a GM is logged in
- WHEN the GM issues `/speed 500`
- THEN the GM's speed MUST become `500`
- AND a `0xA0` stats packet MUST push to the GM's client

### Requirement: Speed Command — Other-Target

The form `/speed "Player" <value>` MUST set the named player's speed.

#### Scenario: Other-target sets speed
- GIVEN a GM and PlayerA exist in the world
- WHEN the GM issues `/speed "PlayerA" 500`
- THEN PlayerA's speed MUST become `500`
- AND a `0xA0` stats packet MUST push to PlayerA

#### Scenario: Invalid player name
- GIVEN a GM issues `/speed "Nobody" 500`
- WHEN no player named "Nobody" exists
- THEN the GM MUST receive an error message
- AND no player speed MUST be modified

### Requirement: Input Validation

The command MUST reject speed values outside `10..=2500`. Non-numeric arguments MUST also be rejected.

#### Scenario: Value below minimum
- GIVEN a GM issues `/speed 5`
- WHEN 5 is below the minimum of `10`
- THEN the GM MUST receive an error
- AND the GM's speed MUST remain unchanged

#### Scenario: Value above maximum
- GIVEN a GM issues `/speed 3000`
- WHEN 3000 exceeds the maximum of `2500`
- THEN the GM MUST receive an error
- AND the GM's speed MUST remain unchanged

#### Scenario: Non-numeric value
- GIVEN a GM issues `/speed fast`
- WHEN the argument is not a valid number
- THEN the GM MUST receive a usage error

### Requirement: Spectator Speed Broadcast

When a player's speed changes, spectators MUST see the update via remove+re-introduce of the affected creature.

#### Scenario: Spectator sees updated speed
- GIVEN spectator S can see target T
- WHEN T's speed changes via `/speed`
- THEN S MUST see T removed and re-introduced
- AND T MUST appear at the correct position with updated speed

#### Scenario: No packets to out-of-range players
- GIVEN player P is outside target T's visual range
- WHEN T's speed changes
- THEN P MUST NOT receive any packets for T

### Requirement: Command Registration

The `/speed` command MUST be registered as `GmVerb::Speed` with canonical word `"speed"` and alias `"spd"`. It MUST appear in `/help` output.

#### Scenario: Help lists speed command
- GIVEN a GM issues `/help`
- THEN the output MUST include `/speed [player] <value>`

#### Scenario: Alias resolves
- GIVEN a GM issues `/spd 500`
- THEN it MUST execute as `/speed 500`
