<?php
// coverage/php84-hooks.php — PHP 8.4 property hooks and asymmetric
// visibility. The no-guess contract: the parser must not panic and must
// classify-or-issue.
//
//   - Plain property hooks (`get`/`set` bodies) and asymmetric visibility
//     (`private(set)`) are consumed as properties; a hooked one is flagged
//     `hooked`. Pinned clean: zero issues.
//   - Hooks on PROMOTED constructor parameters cannot be consumed under the
//     current parameter grammar → one hard ParseIssue, pinned as the
//     founding xfail.txt entry (decision in NOTES.md).
//
// Whatever the parser does here gets pinned either way; this file fails the
// gate only if the behavior *changes* without a deliberate re-bless.

namespace Corp\Hooks;

class HookedProperties
{
    public string $plain = 'plain';

    public string $hooked {
        get => strtoupper($this->plain);
        set {
            $this->plain = trim($value);
        }
    }

    public private(set) string $asymmetric = 'secret';

    public private(set) array $hookedAsymmetric {
        get => array_keys($this->backing);
    }

    private array $backing = ['x'];

    public array $virtual {
        &get => $this->backing;
    }
}

class AsymmetricPromotion
{
    public function __construct(
        public private(set) string $token,
        protected private(set) int $level = 1,
    ) {
    }
}

class HookedPromotion
{
    public function __construct(
        public string $hookedParam {
            get => ucfirst($this->hookedParam);
        },
    ) {
    }
}