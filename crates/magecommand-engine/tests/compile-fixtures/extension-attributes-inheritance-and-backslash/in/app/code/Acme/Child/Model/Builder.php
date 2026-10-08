<?php

namespace Acme\Child\Model;

class Builder
{
    public function __construct(private readonly \Acme\Base\Api\Data\OrderExtensionFactory $factory)
    {
    }
}
