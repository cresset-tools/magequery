<?php

namespace Acme\NewDefault\Model\Value;

/** No constructor, no properties: var_export writes an empty __set_state. */
class Marker
{
    public static function __set_state(array $properties): self
    {
        return new self();
    }
}
