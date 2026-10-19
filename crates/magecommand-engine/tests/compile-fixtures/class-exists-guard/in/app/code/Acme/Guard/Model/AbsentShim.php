<?php
namespace Acme\Guard\Model;

use Magento\Framework\ObjectManagerInterface;
use Vendor\NotInstalled\Model\ThingFactory;

// The alternative-syntax compat shim: the guarded dependency is a generated
// factory whose source class is not installed, so PHP defines the `else`
// class, which has no constructor (metadata row NULL).
if (class_exists(ThingFactory::class)):
    class AbsentShim
    {
        public function __construct(ObjectManagerInterface $objectManager, ThingFactory $thing)
        {
        }
    }
else:
    class AbsentShim
    {
    }
endif;
