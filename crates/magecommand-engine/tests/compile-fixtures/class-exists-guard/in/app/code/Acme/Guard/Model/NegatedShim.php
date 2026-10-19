<?php
namespace Acme\Guard\Model;

// A negated guard on a missing class: the `if` branch is the one that runs.
if (!class_exists('Vendor\NotInstalled\Model\Thing')):
    class NegatedShim
    {
        public function __construct(Dep $dep)
        {
        }
    }
else:
    class NegatedShim
    {
        public function __construct()
        {
        }
    }
endif;
