<?php

namespace Acme\NewDefault\Model\OutOfReach;

use Acme\NewDefault\Model\Value;

/** Out of reach: constructor arguments. NULL row + a finding. */
class WithArguments
{
    public function __construct(Value\Marker $m = new Value\Marker(1))
    {
    }
}
