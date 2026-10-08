<?php

namespace Acme\Time\Model;

// Declares no constructor, so reflection reports DateTime's and the compiler
// folds its defaults: `datetime` => 'now', `timezone` => null.
class Stamp extends \DateTime
{
    public function label(): string
    {
        return $this->format('c');
    }
}
