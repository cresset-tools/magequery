<?php
namespace Acme\Guard\Model;

use Magento\Framework\ObjectManagerInterface;

// Same shape, guarding a factory the generator can create (its source class
// Dep exists), so the `if` class with its constructor is the one defined.
if (class_exists(DepFactory::class)):
    class PresentShim
    {
        public function __construct(ObjectManagerInterface $objectManager, DepFactory $dep)
        {
        }
    }
else:
    class PresentShim
    {
    }
endif;
