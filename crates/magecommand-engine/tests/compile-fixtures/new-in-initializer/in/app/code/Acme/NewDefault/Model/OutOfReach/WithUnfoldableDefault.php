<?php

namespace Acme\NewDefault\Model\OutOfReach;

use Acme\NewDefault\Model\Value;

/** Out of reach: a property default does not fold. NULL row + a finding. */
class WithUnfoldableDefault
{
    public function __construct(Value\Unfoldable $v = new Value\Unfoldable())
    {
    }
}
