<?php

namespace Acme\NewDefault\Model;

use Acme\NewDefault\Model\Value\Child;
use Acme\NewDefault\Model\Value\Marker;

/**
 * PHP 8.1 `new` in initializers. Reflection hands Magento the constructed
 * object, and var_export writes it as `\Fqcn::__set_state(array(…))`.
 */
class Form
{
    public function __construct(
        private readonly array $tree,
        private readonly array $extra = [],
        private readonly Marker $marker = new Marker(),
        private readonly Child $child = new Child
    ) {
    }
}
