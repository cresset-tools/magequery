<?php

namespace Acme\NewDefault\Model\Value;

class Child extends Base
{
    private const TWO = 2;

    public $own = [1, 'a' => [true, null], 'b' => []];
    public $redecl = 'child';
    private $pc = 2.5;
    public ?int $nullable = null;
    /** An int default under a float type is stored as a float... */
    public float $widened = 3;
    /** ...even when it comes from a constant. */
    public float $widenedConst = self::TWO;
    public int|float $union = 5;
    protected $folded = 'b' . parent::SUFFIX;
    public $negative = -1;
}
