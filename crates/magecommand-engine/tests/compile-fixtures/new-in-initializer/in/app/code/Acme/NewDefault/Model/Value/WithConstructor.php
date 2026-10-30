<?php

namespace Acme\NewDefault\Model\Value;

/** A constructor could assign anything: the object is out of reach. */
class WithConstructor
{
    public $set;

    public function __construct()
    {
        $this->set = time();
    }
}
